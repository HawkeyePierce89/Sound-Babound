//! Linux: ALSA for the monitor, PulseAudio for the mic paths.
//!
//! With cpal's `pulseaudio` feature enabled, `cpal::default_host()` prefers
//! PulseAudio. The monitor stays on ALSA's `default` PCM, as before the mic
//! path existed; that PCM follows the PipeWire or PulseAudio default sink.
//! The mic paths need PulseAudio, because only it lists the app's null sink
//! and remapped source by name. PipeWire answers it through pipewire-pulse.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use cpal::HostId;
use cpal::traits::HostTrait;
use pulseaudio::protocol;
use tracing::{error, warn};

use super::shared_host::{Backoff, MAX_STALLED_CONNECTS, SharedHost};

/// The PulseAudio host every mic path shares. A dropped PulseAudio host
/// leaks its reactor thread and connection, so there is only this one.
static MIC_HOST: SharedHost<cpal::Host> = SharedHost::new();

/// Holds PulseAudio connects off after one that failed although the server
/// answered the probe (see [`connect_pulse`]).
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
/// [`CONNECT_BACKOFF`] holds connects off.
///
/// cpal's PulseAudio host gives up on a handshake after 2 s but leaves its
/// connecting thread blocked on the socket, so connecting to a server that
/// accepts connections but never answers would leak one thread per try, and
/// failed builds retry every second. The probe uses a socket of its own
/// with a timeout and leaves nothing behind. It runs inside
/// [`SharedHost::get`]'s connect, under its lock, so probes and connects
/// never overlap. A server can still answer the probe and stall the connect
/// right after it; each such failure holds connects off for longer (up to a
/// minute), and after [`MAX_STALLED_CONNECTS`] stalled connects no
/// PulseAudio connect is tried again in this process (the mic paths fall
/// back to ALSA), so at most that many threads ever leak.
fn connect_pulse() -> Option<cpal::Host> {
    let mut backoff = CONNECT_BACKOFF
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if !backoff.allows(Instant::now()) || !pulse_server_answers() {
        return None;
    }
    let started = Instant::now();
    match cpal::host_from_id(HostId::PulseAudio) {
        Ok(host) => {
            backoff.succeeded();
            Some(host)
        }
        Err(err) => {
            let delay = backoff.failed(Instant::now(), started.elapsed());
            if backoff.exhausted() {
                error!(
                    error = %err,
                    "the PulseAudio server answered the probe but the connect stalled \
                     {MAX_STALLED_CONNECTS} times; not connecting to it again until the app \
                     restarts"
                );
            } else {
                warn!(
                    error = %err,
                    "the PulseAudio server answered the probe but the connect failed; \
                     not connecting again for {delay:?}"
                );
            }
            None
        }
    }
}

/// How long [`pulse_server_answers`] waits for each read and write: cpal's
/// own handshake timeout.
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

/// Sends `command` with sequence number `seq` and reads the reply.
fn roundtrip<T: protocol::CommandReply>(
    socket: &UnixStream,
    reader: &mut BufReader<&UnixStream>,
    seq: u32,
    command: &protocol::Command,
    version: u16,
) -> Answer<T> {
    let mut writer = socket;
    if protocol::write_command_message(&mut writer, seq, command, version).is_err() {
        return Answer::Silent;
    }
    match protocol::read_reply_message::<T>(reader, version) {
        Ok((_, reply)) => Answer::Reply(reply),
        Err(protocol::ProtocolError::Io(_) | protocol::ProtocolError::Timeout) => Answer::Silent,
        Err(_) => Answer::Error,
    }
}

/// Runs the handshake of `pulseaudio::Client::new_unix` (the `Auth`
/// message, then `SetClientName` at the negotiated version) against the
/// server at `path` over a new socket with `timeout` on every read and
/// write. The socket is closed on return, so a server that never answers
/// keeps nothing of this process.
fn server_answers(path: &Path, cookie: Vec<u8>, timeout: Duration) -> bool {
    let Ok(socket) = UnixStream::connect(path) else {
        return false;
    };
    if socket.set_read_timeout(Some(timeout)).is_err()
        || socket.set_write_timeout(Some(timeout)).is_err()
    {
        return false;
    }
    let mut reader = BufReader::new(&socket);
    let auth = protocol::Command::Auth(protocol::AuthParams {
        version: protocol::MAX_VERSION,
        supports_shm: false,
        supports_memfd: false,
        cookie,
    });
    let version = match roundtrip::<protocol::AuthReply>(
        &socket,
        &mut reader,
        0,
        &auth,
        protocol::MAX_VERSION,
    ) {
        Answer::Reply(reply) => protocol::MAX_VERSION.min(reply.version),
        // A refused handshake fails cpal's connect at once, too.
        Answer::Error => return true,
        Answer::Silent => return false,
    };
    let mut props = protocol::Props::new();
    props.set(protocol::Prop::ApplicationName, c"sound-babound probe");
    let name = protocol::Command::SetClientName(props);
    !matches!(
        roundtrip::<protocol::SetClientNameReply>(&socket, &mut reader, 1, &name, version),
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
    use std::io::{Read, Write};
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
    fn a_missing_socket_fails_the_probe() {
        let path = socket_path("missing");
        assert!(!server_answers(&path, Vec::new(), Duration::from_secs(2)));
    }
}
