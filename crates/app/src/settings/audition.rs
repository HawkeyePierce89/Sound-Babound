//! Auditioning a row's sound from the settings window.
//!
//! The sound plays in a child process, `sound-babound play --output-only`,
//! which uses the default output only and never the virtual mic. The pure
//! [`Auditions`] controller keeps at most one audition running; a
//! [`AuditionSpawner`] starts the child ([`ProcessAudition`] in production,
//! a fake in tests).

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::SoundSource;

/// How much of the child's stderr is kept for the failure message.
const STDERR_KEEP: usize = 8 * 1024;

/// How often the waiter thread checks whether the child has exited.
const WAIT_POLL: Duration = Duration::from_millis(10);

/// The argument for `play` that plays the row's sound: `builtin:<name>`
/// for a built-in, otherwise the file's path, a relative one resolved
/// against `config_dir` the way the tray does. An empty path or an unknown
/// built-in is an error to show; nothing is spawned for it.
pub fn audition_target(row_path: &str, config_dir: &Path) -> Result<OsString, String> {
    match SoundSource::resolve(row_path, config_dir)? {
        SoundSource::Builtin(entry) => Ok(OsString::from(entry.path)),
        SoundSource::File(path) => Ok(path.into_os_string()),
    }
}

/// How an audition's child ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitReport {
    pub success: bool,
    /// The exit status, as text.
    pub status: String,
    /// The tail of the child's stderr.
    pub stderr: String,
}

/// The text to show for a failed audition: from the last `Error:` line to
/// the end, joined into one line; otherwise the last non-empty line;
/// otherwise the exit status.
pub fn failure_message(stderr: &str, status: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if let Some(start) = lines.iter().rposition(|line| line.starts_with("Error:")) {
        return lines[start..].join(" ");
    }
    match lines.last() {
        Some(line) => (*line).to_string(),
        None => format!("the sound did not play ({status})"),
    }
}

/// A running audition's child.
pub trait RunningAudition {
    /// Kills the child and reaps it, before returning. Harmless once it has
    /// exited.
    fn kill(&mut self);
}

/// Starts auditions.
pub trait AuditionSpawner {
    /// Starts `play --output-only` for `target`. Once the child has exited,
    /// the spawner reports `(id, ExitReport)` (in production on a channel
    /// the window drains); a failed spawn reports nothing.
    fn spawn(&mut self, id: u64, target: &OsStr) -> Result<Box<dyn RunningAudition>>;
}

struct Current {
    id: u64,
    row: usize,
    child: Box<dyn RunningAudition>,
}

/// At most one audition at a time. Pure: the child is started by `S`.
pub struct Auditions<S> {
    spawner: S,
    next_id: u64,
    current: Option<Current>,
}

impl<S: AuditionSpawner> Auditions<S> {
    pub fn new(spawner: S) -> Auditions<S> {
        Auditions {
            spawner,
            next_id: 0,
            current: None,
        }
    }

    #[cfg(test)]
    pub fn spawner(&self) -> &S {
        &self.spawner
    }

    /// The row whose button reads "Stop".
    pub fn playing(&self) -> Option<usize> {
        self.current.as_ref().map(|current| current.row)
    }

    /// Stops the current audition, then plays `target` for `row`. A failed
    /// spawn leaves nothing playing.
    pub fn play(&mut self, row: usize, target: &OsStr) -> Result<()> {
        self.stop();
        let id = self.next_id;
        self.next_id += 1;
        let child = self.spawner.spawn(id, target)?;
        self.current = Some(Current { id, row, child });
        Ok(())
    }

    /// Kills the current audition, if any.
    pub fn stop(&mut self) {
        if let Some(mut current) = self.current.take() {
            current.child.kill();
        }
    }

    /// The child of audition `id` has exited. Clears the current audition
    /// when the id matches and returns the message for a failure; a stale
    /// id (an audition already killed or replaced) is ignored.
    pub fn exited(&mut self, id: u64, report: ExitReport) -> Option<String> {
        if self.current.as_ref().is_none_or(|current| current.id != id) {
            return None;
        }
        self.current = None;
        (!report.success).then(|| failure_message(&report.stderr, &report.status))
    }

    /// Row `index` was removed from the form: stops its audition, or
    /// shifts the playing row's index when a row above it went.
    pub fn row_removed(&mut self, index: usize) {
        match self.playing() {
            Some(row) if row == index => self.stop(),
            Some(row) if row > index => {
                if let Some(current) = self.current.as_mut() {
                    current.row = row - 1;
                }
            }
            _ => {}
        }
    }

    /// The window is closing: kills any running child.
    pub fn close(&mut self) {
        self.stop();
    }
}

/// Called on the waiter thread after it has sent an exit report, to wake
/// the window.
pub type Repaint = Arc<dyn Fn() + Send + Sync>;

/// Spawns `play --output-only` processes and a waiter thread per child.
pub struct ProcessAudition {
    /// Builds the command for a target (in production, this executable
    /// with `play --output-only -- <target>`).
    command: fn(&OsStr) -> Result<Command>,
    exits: Sender<(u64, ExitReport)>,
    repaint: Repaint,
}

impl ProcessAudition {
    /// A spawner that sends `(id, report)` on `exits` when a child has
    /// exited and then calls `repaint`.
    pub fn new(exits: Sender<(u64, ExitReport)>, repaint: Repaint) -> ProcessAudition {
        ProcessAudition {
            command: audition_command,
            exits,
            repaint,
        }
    }
}

/// This executable with `play --output-only -- <target>`.
fn audition_command(target: &OsStr) -> Result<Command> {
    let exe = std::env::current_exe().context("cannot find the app's executable")?;
    let mut command = Command::new(exe);
    command.args(["play", "--output-only", "--"]).arg(target);
    Ok(command)
}

/// The shared child of one audition: the waiter thread polls it, `kill`
/// kills it, and neither holds the lock while it blocks.
struct ProcessHandle {
    child: Arc<Mutex<Child>>,
}

impl RunningAudition for ProcessHandle {
    fn kill(&mut self) {
        let mut child = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        // An already exited child is fine for `kill`; `wait` reaps it.
        if let Err(err) = child.kill() {
            tracing::debug!("cannot kill the audition: {err}");
        }
        let _ = child.wait();
    }
}

impl Drop for ProcessHandle {
    /// A backstop: a handle that is dropped never leaves a child playing.
    fn drop(&mut self) {
        self.kill();
    }
}

impl AuditionSpawner for ProcessAudition {
    fn spawn(&mut self, id: u64, target: &OsStr) -> Result<Box<dyn RunningAudition>> {
        let mut command = (self.command)(target)?;
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot start the audition")?;
        let stderr = child.stderr.take();
        let child = Arc::new(Mutex::new(child));
        let mut handle = ProcessHandle {
            child: Arc::clone(&child),
        };
        let exits = self.exits.clone();
        let repaint = Arc::clone(&self.repaint);
        let waiter = thread::Builder::new()
            .name("audition-waiter".to_string())
            .spawn(move || {
                let tail = stderr.map(read_tail).unwrap_or_default();
                let report = wait_for_exit(&child, &tail);
                // The window may be gone already.
                if exits.send((id, report)).is_ok() {
                    repaint();
                }
            });
        if let Err(err) = waiter {
            handle.kill();
            return Err(anyhow::Error::new(err).context("cannot start the audition waiter thread"));
        }
        Ok(Box::new(handle))
    }
}

/// Reads `source` to the end and returns the last [`STDERR_KEEP`] bytes
/// as text.
fn read_tail(mut source: impl Read) -> String {
    let mut kept: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        match source.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                kept.extend_from_slice(&buffer[..n]);
                if kept.len() > STDERR_KEEP {
                    kept.drain(..kept.len() - STDERR_KEEP);
                }
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// Waits for the child to exit without holding its lock while it runs,
/// so that `kill` can always take it.
fn wait_for_exit(child: &Mutex<Child>, stderr: &str) -> ExitReport {
    loop {
        let polled = child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_wait();
        match polled {
            Ok(Some(status)) => {
                return ExitReport {
                    success: status.success(),
                    status: status.to_string(),
                    stderr: stderr.to_string(),
                };
            }
            Ok(None) => thread::sleep(WAIT_POLL),
            Err(err) => {
                return ExitReport {
                    success: false,
                    status: format!("cannot wait for the audition: {err}"),
                    stderr: stderr.to_string(),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        Spawn(u64, OsString),
        Kill(u64),
    }

    #[derive(Default, Clone)]
    struct FakeSpawner {
        events: Rc<RefCell<Vec<Event>>>,
        fail: Rc<RefCell<bool>>,
    }

    struct FakeChild {
        id: u64,
        events: Rc<RefCell<Vec<Event>>>,
    }

    impl RunningAudition for FakeChild {
        fn kill(&mut self) {
            self.events.borrow_mut().push(Event::Kill(self.id));
        }
    }

    impl AuditionSpawner for FakeSpawner {
        fn spawn(&mut self, id: u64, target: &OsStr) -> Result<Box<dyn RunningAudition>> {
            if *self.fail.borrow() {
                return Err(anyhow::anyhow!("no such program"));
            }
            self.events
                .borrow_mut()
                .push(Event::Spawn(id, target.to_os_string()));
            Ok(Box::new(FakeChild {
                id,
                events: Rc::clone(&self.events),
            }))
        }
    }

    fn auditions() -> (Auditions<FakeSpawner>, FakeSpawner) {
        let spawner = FakeSpawner::default();
        (Auditions::new(spawner.clone()), spawner)
    }

    fn ok() -> ExitReport {
        ExitReport {
            success: true,
            status: "exit status: 0".into(),
            stderr: String::new(),
        }
    }

    fn failed(stderr: &str) -> ExitReport {
        ExitReport {
            success: false,
            status: "exit status: 1".into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn play_starts_the_row_with_the_target() {
        let (mut auditions, spawner) = auditions();
        assert_eq!(auditions.playing(), None);
        auditions.play(2, OsStr::new("builtin:airhorn")).unwrap();
        assert_eq!(auditions.playing(), Some(2));
        assert_eq!(
            *spawner.events.borrow(),
            [Event::Spawn(0, "builtin:airhorn".into())]
        );
    }

    #[test]
    fn stop_kills_the_child() {
        let (mut auditions, spawner) = auditions();
        auditions.play(0, OsStr::new("a")).unwrap();
        auditions.stop();
        assert_eq!(auditions.playing(), None);
        assert_eq!(spawner.events.borrow().last(), Some(&Event::Kill(0)));
        // Nothing left to stop.
        auditions.stop();
        assert_eq!(spawner.events.borrow().len(), 2);
    }

    #[test]
    fn switching_rows_kills_the_first_before_spawning_the_second() {
        let (mut auditions, spawner) = auditions();
        auditions.play(0, OsStr::new("a")).unwrap();
        auditions.play(3, OsStr::new("b")).unwrap();
        assert_eq!(auditions.playing(), Some(3));
        assert_eq!(
            *spawner.events.borrow(),
            [
                Event::Spawn(0, "a".into()),
                Event::Kill(0),
                Event::Spawn(1, "b".into()),
            ]
        );
    }

    #[test]
    fn an_observed_exit_clears_playing_and_reports_failures() {
        let (mut auditions, _) = auditions();
        auditions.play(1, OsStr::new("a")).unwrap();
        assert_eq!(auditions.exited(0, ok()), None);
        assert_eq!(auditions.playing(), None);

        auditions.play(1, OsStr::new("a")).unwrap();
        let message = auditions
            .exited(1, failed("Error: cannot decode\nCaused by: bad\n"))
            .unwrap();
        assert_eq!(message, "Error: cannot decode Caused by: bad");
        assert_eq!(auditions.playing(), None);
    }

    #[test]
    fn a_stale_exit_after_a_kill_is_ignored() {
        let (mut auditions, _) = auditions();
        auditions.play(0, OsStr::new("a")).unwrap();
        auditions.stop();
        assert_eq!(auditions.exited(0, failed("Error: killed")), None);
        // A stale exit does not clear a newer audition either.
        auditions.play(1, OsStr::new("b")).unwrap();
        assert_eq!(auditions.exited(0, failed("Error: old")), None);
        assert_eq!(auditions.playing(), Some(1));
    }

    #[test]
    fn a_failed_spawn_leaves_nothing_playing() {
        let (mut auditions, spawner) = auditions();
        auditions.play(0, OsStr::new("a")).unwrap();
        *spawner.fail.borrow_mut() = true;
        let err = auditions.play(1, OsStr::new("b")).unwrap_err();
        assert!(err.to_string().contains("no such program"));
        assert_eq!(auditions.playing(), None);
        // The old child was killed before the spawn was tried.
        assert_eq!(spawner.events.borrow().last(), Some(&Event::Kill(0)));
    }

    #[test]
    fn removing_rows_stops_or_shifts_the_playing_row() {
        let (mut auditions, spawner) = auditions();
        auditions.play(2, OsStr::new("a")).unwrap();
        auditions.row_removed(3);
        assert_eq!(auditions.playing(), Some(2));
        auditions.row_removed(0);
        assert_eq!(auditions.playing(), Some(1));
        auditions.row_removed(1);
        assert_eq!(auditions.playing(), None);
        assert_eq!(spawner.events.borrow().last(), Some(&Event::Kill(0)));
        // With nothing playing, a removal changes nothing.
        auditions.row_removed(0);
        assert_eq!(spawner.events.borrow().len(), 2);
    }

    #[test]
    fn close_kills_the_running_child() {
        let (mut auditions, spawner) = auditions();
        auditions.play(0, OsStr::new("a")).unwrap();
        auditions.close();
        assert_eq!(auditions.playing(), None);
        assert_eq!(spawner.events.borrow().last(), Some(&Event::Kill(0)));
        assert_eq!(auditions.spawner().events.borrow().len(), 2);
    }

    #[test]
    fn target_of_a_builtin_is_its_path_whatever_the_case() {
        let dir = Path::new("/cfg");
        assert_eq!(
            audition_target("builtin:airhorn", dir),
            Ok(OsString::from("builtin:airhorn"))
        );
        assert_eq!(
            audition_target(" BUILTIN:AirHorn ", dir),
            Ok(OsString::from("builtin:airhorn"))
        );
    }

    #[test]
    fn target_of_a_file_is_absolute() {
        let dir = std::env::temp_dir().join("cfg");
        assert_eq!(
            audition_target("sounds/a.wav", &dir),
            Ok(dir.join("sounds/a.wav").into_os_string())
        );
        let abs = std::env::temp_dir().join("x.wav");
        assert_eq!(
            audition_target(abs.to_str().unwrap(), &dir),
            Ok(abs.clone().into_os_string())
        );
    }

    #[test]
    fn target_errors_for_an_unknown_built_in_and_an_empty_path() {
        let dir = Path::new("/cfg");
        let err = audition_target("builtin:nope", dir).unwrap_err();
        assert!(err.contains("builtin:nope") && err.contains("builtin:airhorn"));
        assert!(audition_target("  ", dir).is_err());
    }

    #[test]
    fn failure_message_takes_the_last_error_line_to_the_end() {
        let stderr = "INFO starting\nError: cannot load \"x\"\n\nCaused by:\n  no such file\n";
        assert_eq!(
            failure_message(stderr, "exit status: 1"),
            "Error: cannot load \"x\" Caused by: no such file"
        );
        let two = "Error: first\nError: second\nCaused by: why\n";
        assert_eq!(failure_message(two, "s"), "Error: second Caused by: why");
    }

    #[test]
    fn failure_message_falls_back_to_the_last_line_then_the_status() {
        assert_eq!(
            failure_message("a\n\nsomething broke\n\n", "exit status: 1"),
            "something broke"
        );
        let message = failure_message("  \n", "exit status: 101");
        assert!(message.contains("exit status: 101"), "{message}");
    }

    #[test]
    fn read_tail_keeps_only_the_last_bytes() {
        let long = "x".repeat(STDERR_KEEP * 2) + "end";
        let tail = read_tail(long.as_bytes());
        assert_eq!(tail.len(), STDERR_KEEP);
        assert!(tail.ends_with("end"));
        assert_eq!(read_tail("short".as_bytes()), "short");
    }

    /// Runs this test binary listing no test: a harmless process that exits
    /// at once and prints nothing.
    fn quiet_child(_target: &OsStr) -> Result<Command> {
        let mut command = Command::new(std::env::current_exe()?);
        command.args(["--list", "--exact", "no-such-test"]);
        Ok(command)
    }

    fn missing_program(_target: &OsStr) -> Result<Command> {
        Ok(Command::new("/nonexistent/sound-babound-audition-test"))
    }

    type Reports = mpsc::Receiver<(u64, ExitReport)>;

    fn process_audition(
        command: fn(&OsStr) -> Result<Command>,
    ) -> (ProcessAudition, Reports, Arc<AtomicUsize>) {
        let (send, receive) = mpsc::channel();
        let repaints = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&repaints);
        let spawner = ProcessAudition {
            command,
            exits: send,
            repaint: Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        };
        (spawner, receive, repaints)
    }

    #[test]
    fn the_process_audition_reports_the_exit_once() {
        let (mut spawner, reports, repaints) = process_audition(quiet_child);
        let mut child = spawner.spawn(7, OsStr::new("ignored")).unwrap();
        let (id, report) = reports
            .recv_timeout(Duration::from_secs(10))
            .expect("the exit was never reported");
        assert_eq!(id, 7);
        assert!(report.success, "{report:?}");
        // The repaint comes right after the report was sent.
        let deadline = Instant::now() + Duration::from_secs(10);
        while repaints.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "no repaint was asked for");
            thread::sleep(Duration::from_millis(5));
        }
        // The waiter has ended: it reports once.
        thread::sleep(Duration::from_millis(100));
        assert!(reports.try_recv().is_err());
        assert_eq!(repaints.load(Ordering::SeqCst), 1);
        // Kill after the exit is harmless, also twice.
        child.kill();
        child.kill();
    }

    #[test]
    fn a_missing_program_fails_to_spawn_and_reports_nothing() {
        let (mut spawner, reports, repaints) = process_audition(missing_program);
        let Err(err) = spawner.spawn(0, OsStr::new("x")) else {
            panic!("the spawn should fail");
        };
        assert!(format!("{err:#}").contains("cannot start the audition"));
        drop(spawner);
        // The sender is gone with the spawner: nothing was ever sent.
        assert!(reports.recv_timeout(Duration::from_secs(5)).is_err());
        assert_eq!(repaints.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_production_command_plays_on_the_output_only() {
        let command = audition_command(OsStr::new("builtin:airhorn")).unwrap();
        assert_eq!(command.get_program(), std::env::current_exe().unwrap());
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["play", "--output-only", "--", "builtin:airhorn"]);
    }
}
