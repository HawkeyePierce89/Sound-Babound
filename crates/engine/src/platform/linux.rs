//! Linux: ALSA for the monitor, PulseAudio for the mic paths.
//!
//! With cpal's `pulseaudio` feature enabled, `cpal::default_host()` prefers
//! PulseAudio. The monitor stays on ALSA's `default` PCM, as before the mic
//! path existed; that PCM follows the PipeWire or PulseAudio default sink.
//! The mic paths need PulseAudio, because only it lists the app's null sink
//! and remapped source by name. PipeWire answers it through pipewire-pulse.

use std::io::{self, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use cpal::HostId;
use cpal::traits::HostTrait;
use pulseaudio::protocol;
use socket2::{Domain, SockAddr, Socket, Type};
use tracing::{debug, error, warn};

use super::shared_host::{Backoff, ConnectOutcome, MAX_STALLED_CONNECTS, SharedHost, try_connect};

/// The PulseAudio host every mic path shares. A dropped PulseAudio host
/// leaks its reactor thread and connection, so there is only this one.
static MIC_HOST: SharedHost<cpal::Host> = SharedHost::new();

/// Holds PulseAudio probes and connects off after a failed probe, or a
/// connect that failed although the server answered the probe (see
/// [`connect_pulse`]).
static CONNECT_BACKOFF: Mutex<Backoff> = Mutex::new(Backoff::new());

/// The monitor's host: ALSA, or the shared PulseAudio host if ALSA cannot
/// open. Never cpal's default host, which would connect a PulseAudio host
/// of its own (unprobed, and leaked when dropped) on every build.
pub fn monitor_host() -> Option<Arc<cpal::Host>> {
    match cpal::host_from_id(HostId::Alsa) {
        Ok(host) => Some(Arc::new(host)),
        Err(_) => shared_pulse_host(),
    }
}

/// The mic paths' host: the process-wide PulseAudio host, reconnected only
/// if its connection no longer answers (a query on a dead connection fails
/// at once), or ALSA if no PulseAudio server answers. The fallback is not
/// kept, so the next call tries PulseAudio again. `None` if neither opens.
///
/// The fallback is ALSA by id, never cpal's default host: that tries
/// PulseAudio first, without the probe, and a PulseAudio host outside
/// [`MIC_HOST`] would leak its reactor thread and connection when dropped.
pub fn mic_host() -> Option<Arc<cpal::Host>> {
    shared_pulse_host().or_else(|| cpal::host_from_id(HostId::Alsa).ok().map(Arc::new))
}

/// The kept PulseAudio host, or a new one from [`connect_pulse`].
fn shared_pulse_host() -> Option<Arc<cpal::Host>> {
    let alive = |host: &cpal::Host| host.devices().is_ok();
    MIC_HOST.get(alive, connect_pulse)
}

/// Connects a PulseAudio host, but only after [`pulse_server_answers`] saw
/// the server answer both messages of the handshake, and not while
/// [`CONNECT_BACKOFF`] holds probes and connects off.
///
/// cpal's PulseAudio host gives up on a handshake after 2 s but leaves its
/// connecting thread blocked on the socket, so connecting to a server that
/// accepts connections but never answers would leak one thread per try, and
/// failed builds retry every second. The probe uses a socket of its own
/// with a timeout and leaves nothing behind. It runs inside
/// [`SharedHost::get`]'s connect, under its lock, so probes and connects
/// never overlap. A failed probe holds further probes off (doubling up to
/// [`PROBE_HOLD_OFF_MAX`](super::shared_host::PROBE_HOLD_OFF_MAX), 10 s),
/// since a timed-out one blocks every mic caller behind that lock for at
/// most the probe's deadline ([`PROBE_TIMEOUT`]) and the pollers ask every
/// second. A server can still answer the probe and stall the connect right
/// after it; each such failure holds connects off for longer (doubling up to
/// a minute, and a probe failure after one keeps that hold-off), and after
/// [`MAX_STALLED_CONNECTS`] stalled connects no PulseAudio connect is tried
/// again in this process (the mic paths fall back to ALSA), so at most that
/// many threads ever leak.
fn connect_pulse() -> Option<cpal::Host> {
    let mut backoff = CONNECT_BACKOFF
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    match try_connect(&mut backoff, Instant::now, pulse_server_answers, || {
        cpal::host_from_id(HostId::PulseAudio)
    }) {
        ConnectOutcome::Connected(host) => Some(host),
        ConnectOutcome::HeldOff => None,
        // At debug: the 1 Hz pollers would flood the log while no server runs.
        ConnectOutcome::ProbeFailed { delay } => {
            debug!("no PulseAudio server answered the probe; not probing again for {delay:?}");
            None
        }
        ConnectOutcome::Failed {
            error,
            exhausted: true,
            ..
        } => {
            error!(
                error = %error,
                "the PulseAudio server answered the probe but the connect stalled \
                 {MAX_STALLED_CONNECTS} times; not connecting to it again until the app \
                 restarts"
            );
            None
        }
        ConnectOutcome::Failed { error, delay, .. } => {
            warn!(
                error = %error,
                "the PulseAudio server answered the probe but the connect failed; \
                 not connecting again for {delay:?}"
            );
            None
        }
    }
}

/// The deadline of the whole probe in [`pulse_server_answers`], the connect
/// and both handshake messages together: cpal's own handshake timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether the PulseAudio server the environment names answers both
/// messages of the handshake cpal's connect makes, `Auth` and
/// `SetClientName` (an error reply counts: the server is answering, and a
/// connect then fails at once instead of hanging).
fn pulse_server_answers() -> bool {
    let Some(path) = pulseaudio::socket_path_from_env() else {
        return false;
    };
    let cookie = pulseaudio::cookie_path_from_env()
        .and_then(|path| std::fs::read(path).ok())
        .unwrap_or_default();
    server_answers(&path, cookie, PROBE_TIMEOUT)
}

/// What the server did with one handshake message.
enum Answer<T> {
    /// It replied.
    Reply(T),
    /// It answered with an error.
    Error,
    /// It did not answer in time, or it closed the connection.
    Silent,
}

/// A stream that fails with `TimedOut` once its deadline has passed, and
/// before every call limits the socket's read or write timeout to the time
/// left. One deadline then holds over a whole exchange, even against a
/// server that answers a byte at a time (a fixed timeout restarts on every
/// byte).
#[derive(Clone, Copy)]
struct Deadlined<'a> {
    socket: &'a UnixStream,
    deadline: Instant,
}

impl Deadlined<'_> {
    /// The time left, or `TimedOut` if there is none (a zero socket timeout
    /// is not allowed, and means no timeout elsewhere).
    fn left(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::ErrorKind::TimedOut.into())
    }
}

impl Read for Deadlined<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket.set_read_timeout(Some(self.left()?))?;
        self.socket.read(buf)
    }
}

impl Write for Deadlined<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket.set_write_timeout(Some(self.left()?))?;
        self.socket.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Sends `command` with sequence number `seq` and reads the reply.
fn roundtrip<T: protocol::CommandReply>(
    reader: &mut BufReader<Deadlined<'_>>,
    seq: u32,
    command: &protocol::Command,
    version: u16,
) -> Answer<T> {
    let mut writer = *reader.get_ref();
    if protocol::write_command_message(&mut writer, seq, command, version).is_err() {
        return Answer::Silent;
    }
    match protocol::read_reply_message::<T>(reader, version) {
        Ok((_, reply)) => Answer::Reply(reply),
        Err(protocol::ProtocolError::Io(_) | protocol::ProtocolError::Timeout) => Answer::Silent,
        Err(_) => Answer::Error,
    }
}

/// How often [`connect_within`] checks a connect still in progress.
const CONNECT_POLL: Duration = Duration::from_millis(5);

/// Connects a stream socket to `path` without ever blocking past `deadline`,
/// and returns it in blocking mode (the caller sets read and write timeouts).
/// A plain blocking connect waits, without a timeout, while the listener's
/// backlog is full (a server that stopped accepting).
///
/// The connect is non-blocking: an immediate `EAGAIN` (the backlog is full)
/// fails at once, and an `EINPROGRESS` is waited out until `deadline`,
/// checking the socket every [`CONNECT_POLL`] (a Unix socket connect does
/// not normally report it, so this is a fallback, kept free of `unsafe`).
fn connect_within(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    match socket.connect(&SockAddr::unix(path)?) {
        Ok(()) => {}
        Err(err) if err.raw_os_error() == Some(libc::EINPROGRESS) => {
            wait_connected(&socket, deadline)?;
        }
        // `WouldBlock` (`EAGAIN`) included: the server accepts no one.
        Err(err) => return Err(err),
    }
    socket.set_nonblocking(false)?;
    Ok(UnixStream::from(OwnedFd::from(socket)))
}

/// Waits until `deadline` for a connect in progress on `socket` to finish.
fn wait_connected(socket: &Socket, deadline: Instant) -> io::Result<()> {
    loop {
        if let Some(err) = socket.take_error()? {
            return Err(err);
        }
        if socket.peer_addr().is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(CONNECT_POLL);
    }
}

/// Runs the handshake of `pulseaudio::Client::new_unix` (the `Auth`
/// message, then `SetClientName` at the negotiated version) against the
/// server at `path` over a new socket. One deadline, `timeout` from now,
/// covers the connect ([`connect_within`]) and both messages, so the probe
/// takes at most `timeout` plus one [`CONNECT_POLL`], however slowly the
/// server answers. The socket is closed on return, so a server that never
/// answers keeps nothing of this process.
fn server_answers(path: &Path, cookie: Vec<u8>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let Ok(socket) = connect_within(path, deadline) else {
        return false;
    };
    let mut reader = BufReader::new(Deadlined {
        socket: &socket,
        deadline,
    });
    let auth = protocol::Command::Auth(protocol::AuthParams {
        version: protocol::MAX_VERSION,
        supports_shm: false,
        supports_memfd: false,
        cookie,
    });
    let version =
        match roundtrip::<protocol::AuthReply>(&mut reader, 0, &auth, protocol::MAX_VERSION) {
            Answer::Reply(reply) => protocol::MAX_VERSION.min(reply.version),
            // A refused handshake fails cpal's connect at once, too.
            Answer::Error => return true,
            Answer::Silent => return false,
        };
    let mut props = protocol::Props::new();
    props.set(protocol::Prop::ApplicationName, c"sound-babound probe");
    let name = protocol::Command::SetClientName(props);
    !matches!(
        roundtrip::<protocol::SetClientNameReply>(&mut reader, 1, &name, version),
        Answer::Silent
    )
}

/// The mic paths' stream period: 10 ms. With the default buffer size cpal
/// lets the PulseAudio server choose, and PulseAudio and pipewire-pulse then
/// use about 2 s per fragment: the capture would arrive in 2 s blocks (far
/// past the passthrough's high mark) and the mic sink would ask for 2 s at a
/// time (an underrun every block).
pub const MIC_PERIOD_MS: Option<u32> = Some(10);

/// The virtual device the real-device smoke test looks for: the null sink
/// the app creates.
#[cfg(test)]
pub const SMOKE_MIC_PATTERN: &str = "sound_babound_sink";

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::time::Instant;

    use super::*;

    /// A socket path in a fresh directory under the temp dir.
    fn socket_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("babound-probe-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("native")
    }

    #[test]
    fn a_server_that_never_answers_fails_the_probe_in_time() {
        let path = socket_path("silent");
        let listener = UnixListener::bind(&path).expect("bind");
        let server = std::thread::spawn(move || {
            // Accept and read, but never reply, until the client hangs up.
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 256];
            while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
        });
        let started = Instant::now();
        assert!(!server_answers(
            &path,
            Vec::new(),
            Duration::from_millis(200)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().expect("server");
    }

    #[test]
    fn a_server_that_hangs_up_fails_the_probe() {
        let path = socket_path("hangup");
        let listener = UnixListener::bind(&path).expect("bind");
        let server = std::thread::spawn(move || drop(listener.accept()));
        assert!(!server_answers(&path, Vec::new(), Duration::from_secs(2)));
        server.join().expect("server");
    }

    #[test]
    fn an_error_reply_counts_as_answering() {
        let path = socket_path("error");
        let listener = UnixListener::bind(&path).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let (seq, _) =
                protocol::read_command_message(&mut reader, protocol::MAX_VERSION).expect("auth");
            protocol::write_error(&mut stream, seq, &protocol::PulseError::AccessDenied)
                .expect("reply");
            stream.flush().expect("flush");
        });
        assert!(server_answers(&path, Vec::new(), Duration::from_secs(2)));
        server.join().expect("server");
    }

    /// Answers the `Auth` message on `stream` with a reply at the newest
    /// version and returns the reader for what follows.
    fn accept_auth(stream: &UnixStream) -> BufReader<UnixStream> {
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let (seq, _) =
            protocol::read_command_message(&mut reader, protocol::MAX_VERSION).expect("auth");
        let reply = protocol::AuthReply {
            version: protocol::MAX_VERSION,
            ..Default::default()
        };
        let mut writer = stream;
        protocol::write_reply_message(&mut writer, seq, &reply, protocol::MAX_VERSION)
            .expect("reply");
        reader
    }

    #[test]
    fn a_server_that_stalls_after_auth_fails_the_probe_in_time() {
        let path = socket_path("stall");
        let listener = UnixListener::bind(&path).expect("bind");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = accept_auth(&stream);
            // Read SetClientName and the rest, but never reply.
            let mut buf = [0u8; 256];
            while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
        });
        let started = Instant::now();
        assert!(!server_answers(
            &path,
            Vec::new(),
            Duration::from_millis(200)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().expect("server");
    }

    #[test]
    fn a_server_that_completes_the_handshake_passes_the_probe() {
        let path = socket_path("full");
        let listener = UnixListener::bind(&path).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = accept_auth(&stream);
            let (seq, command) = protocol::read_command_message(&mut reader, protocol::MAX_VERSION)
                .expect("set client name");
            assert!(matches!(command, protocol::Command::SetClientName(_)));
            let reply = protocol::SetClientNameReply { client_id: 7 };
            protocol::write_reply_message(&mut stream, seq, &reply, protocol::MAX_VERSION)
                .expect("reply");
            stream.flush().expect("flush");
        });
        assert!(server_answers(&path, Vec::new(), Duration::from_secs(2)));
        server.join().expect("server");
    }

    #[test]
    fn a_server_that_answers_slowly_fails_the_probe_within_one_deadline() {
        let path = socket_path("slow");
        let listener = UnixListener::bind(&path).expect("bind");
        let timeout = Duration::from_millis(500);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let (seq, _) =
                protocol::read_command_message(&mut reader, protocol::MAX_VERSION).expect("auth");
            // Slow, but inside the timeout: a fixed timeout per message
            // would give the next message a fresh one.
            std::thread::sleep(timeout * 7 / 10);
            let reply = protocol::AuthReply {
                version: protocol::MAX_VERSION,
                ..Default::default()
            };
            let mut writer = &stream;
            protocol::write_reply_message(&mut writer, seq, &reply, protocol::MAX_VERSION)
                .expect("reply");
            // Read SetClientName and the rest, but never reply.
            let mut buf = [0u8; 256];
            while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
        });
        let started = Instant::now();
        assert!(!server_answers(&path, Vec::new(), timeout));
        let took = started.elapsed();
        assert!(took < timeout + Duration::from_millis(300), "took {took:?}");
        server.join().expect("server");
    }

    #[test]
    fn a_server_that_trickles_its_reply_fails_the_probe_within_one_deadline() {
        let path = socket_path("trickle");
        let listener = UnixListener::bind(&path).expect("bind");
        let timeout = Duration::from_millis(500);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let (seq, _) =
                protocol::read_command_message(&mut reader, protocol::MAX_VERSION).expect("auth");
            let reply = protocol::AuthReply {
                version: protocol::MAX_VERSION,
                ..Default::default()
            };
            let mut bytes = Vec::new();
            protocol::write_reply_message(&mut bytes, seq, &reply, protocol::MAX_VERSION)
                .expect("serialize");
            // One byte every 100 ms: each is well inside the timeout, but
            // the whole reply takes several times as long.
            let mut writer = &stream;
            for byte in bytes {
                if writer.write_all(&[byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let started = Instant::now();
        assert!(!server_answers(&path, Vec::new(), timeout));
        let took = started.elapsed();
        assert!(took < timeout + Duration::from_millis(300), "took {took:?}");
        server.join().expect("server");
    }

    #[test]
    fn a_server_whose_backlog_is_full_fails_the_probe_in_time() {
        let path = socket_path("backlog");
        // A listener that never accepts, with the smallest backlog.
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None).expect("socket");
        listener
            .bind(&SockAddr::unix(&path).expect("address"))
            .expect("bind");
        listener.listen(0).expect("listen");
        // Fill the backlog with non-blocking connects until one would block.
        let mut queued = Vec::new();
        loop {
            assert!(queued.len() < 1024, "the backlog never filled");
            let client = Socket::new(Domain::UNIX, Type::STREAM, None).expect("socket");
            client.set_nonblocking(true).expect("non-blocking");
            match client.connect(&SockAddr::unix(&path).expect("address")) {
                Ok(()) => queued.push(client),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("unexpected connect error: {err}"),
            }
        }

        // On a thread, so a connect that blocks fails the test instead of
        // hanging it.
        let timeout = Duration::from_millis(500);
        let (done, result) = std::sync::mpsc::channel();
        let probe_path = path.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let answered = server_answers(&probe_path, Vec::new(), timeout);
            let _ = done.send((answered, started.elapsed()));
        });
        let (answered, took) = result
            .recv_timeout(timeout + Duration::from_secs(2))
            .expect("the probe blocked on a full backlog");
        assert!(!answered);
        assert!(took < timeout + Duration::from_secs(1), "took {took:?}");
        drop(queued);
        drop(listener);
    }

    #[test]
    fn waiting_on_a_connect_that_never_completes_times_out() {
        // A socket that was never connected has no peer and no error, as a
        // connect still in progress has.
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None).expect("socket");
        let timeout = Duration::from_millis(100);
        let started = Instant::now();
        let err = wait_connected(&socket, started + timeout).expect_err("never connected");
        let took = started.elapsed();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(took >= timeout, "took {took:?}");
        assert!(took < timeout + Duration::from_secs(1), "took {took:?}");
    }

    #[test]
    fn a_missing_socket_fails_the_probe() {
        let path = socket_path("missing");
        assert!(!server_answers(&path, Vec::new(), Duration::from_secs(2)));
    }
}
