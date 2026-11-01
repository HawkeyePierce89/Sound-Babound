//! The tray's launcher for the settings window, which runs as a separate
//! process (`sound-babound settings`).
//!
//! [`SettingsLauncher`] is the pure controller: one window at a time, and a
//! reload after it closes only if the config file's bytes changed. The
//! process itself is started by a [`SettingsSpawner`]: [`ProcessSpawner`]
//! in production, a fake in tests.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;

use anyhow::{Context, Result, anyhow};

/// Starts the settings window.
pub trait SettingsSpawner {
    /// Starts it. Once it has exited, the spawner reports that (in
    /// production as `UserEvent::SettingsClosed`), so that the tray calls
    /// [`SettingsLauncher::closed`]. A failed spawn reports nothing.
    fn spawn(&mut self) -> Result<()>;
}

/// Called on the waiter thread once the settings process has exited.
pub type OnExit = Arc<dyn Fn() + Send + Sync>;

/// Spawns the settings process and a waiter thread per window, which waits
/// for the process and then calls `on_exit`.
pub struct ProcessSpawner {
    /// Builds the command to run (in production, this executable with
    /// `settings`).
    command: fn() -> Result<Command>,
    on_exit: OnExit,
}

impl ProcessSpawner {
    /// A spawner of `sound-babound settings` that calls `on_exit` when the
    /// window's process has exited.
    pub fn new(on_exit: OnExit) -> ProcessSpawner {
        ProcessSpawner {
            command: settings_command,
            on_exit,
        }
    }
}

/// This executable with `settings`, its output going where the tray's goes.
fn settings_command() -> Result<Command> {
    let exe = std::env::current_exe().context("cannot find the app's executable")?;
    let mut command = Command::new(exe);
    command
        .arg("settings")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    Ok(command)
}

impl SettingsSpawner for ProcessSpawner {
    /// The waiter thread starts first and gets the child once it runs, so a
    /// running window always has a waiter: a window nobody waits for would
    /// keep the tray's hotkeys paused for good. If the process fails to
    /// start, the thread ends without calling `on_exit`.
    fn spawn(&mut self) -> Result<()> {
        let (send_child, receive_child) = mpsc::channel::<Child>();
        let on_exit = Arc::clone(&self.on_exit);
        thread::Builder::new()
            .name("settings-waiter".to_string())
            .spawn(move || {
                let Ok(mut child) = receive_child.recv() else {
                    return;
                };
                match child.wait() {
                    Ok(status) => tracing::debug!(%status, "the settings window exited"),
                    Err(err) => tracing::warn!("cannot wait for the settings window: {err}"),
                }
                on_exit();
            })
            .context("cannot start the settings waiter thread")?;
        let mut command = (self.command)()?;
        let child = command
            .stdin(Stdio::null())
            .spawn()
            .context("cannot launch the settings window")?;
        if let Err(mpsc::SendError(mut child)) = send_child.send(child) {
            // The waiter is gone: nobody would report the close.
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("the settings waiter thread ended early"));
        }
        Ok(())
    }
}

/// The config file's bytes, for [`SettingsLauncher::open`] and
/// [`SettingsLauncher::closed`]. `None` when there is no path or the file
/// cannot be read.
pub fn config_snapshot(path: Option<&Path>) -> Option<Vec<u8>> {
    path.and_then(|path| fs::read(path).ok())
}

/// What [`SettingsLauncher::open`] did.
#[derive(Debug)]
pub enum OpenOutcome {
    /// The window was started; the tray pauses its hotkeys.
    Launched,
    /// A window is already open; nothing was started.
    AlreadyOpen,
    /// Starting it failed; the launcher stays closed and can be opened again.
    Failed(anyhow::Error),
}

/// One settings window at a time, and whether its close needs a reload.
/// Pure: the process is started by `S`.
pub struct SettingsLauncher<S> {
    spawner: S,
    /// The config file's bytes from before the running window was started;
    /// `None` while no window runs.
    running: Option<Option<Vec<u8>>>,
}

impl<S: SettingsSpawner> SettingsLauncher<S> {
    pub fn new(spawner: S) -> SettingsLauncher<S> {
        SettingsLauncher {
            spawner,
            running: None,
        }
    }

    #[cfg(test)]
    pub fn spawner(&self) -> &S {
        &self.spawner
    }

    /// Whether a settings window runs.
    pub fn is_open(&self) -> bool {
        self.running.is_some()
    }

    /// Starts the window unless one runs. `snapshot` is the config file's
    /// bytes, read before the spawn ([`config_snapshot`]).
    pub fn open(&mut self, snapshot: Option<Vec<u8>>) -> OpenOutcome {
        if self.is_open() {
            return OpenOutcome::AlreadyOpen;
        }
        match self.spawner.spawn() {
            Ok(()) => {
                self.running = Some(snapshot);
                OpenOutcome::Launched
            }
            Err(err) => OpenOutcome::Failed(err),
        }
    }

    /// The window has closed; `current` is the config file's bytes now.
    /// Returns whether to reload: only when they differ from the snapshot
    /// (a file that was missing before and after has not changed). A close
    /// with no window open reloads nothing.
    pub fn closed(&mut self, current: Option<Vec<u8>>) -> bool {
        match self.running.take() {
            Some(snapshot) => snapshot != current,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::test_support::TempDir;

    /// Counts spawns; fails while `fail` is set.
    #[derive(Default, Clone)]
    struct FakeSpawner {
        spawns: Rc<Cell<usize>>,
        fail: Rc<Cell<bool>>,
    }

    impl SettingsSpawner for FakeSpawner {
        fn spawn(&mut self) -> Result<()> {
            if self.fail.get() {
                return Err(anyhow!("no such program"));
            }
            self.spawns.set(self.spawns.get() + 1);
            Ok(())
        }
    }

    fn launcher() -> (SettingsLauncher<FakeSpawner>, FakeSpawner) {
        let spawner = FakeSpawner::default();
        (SettingsLauncher::new(spawner.clone()), spawner)
    }

    fn bytes(text: &str) -> Option<Vec<u8>> {
        Some(text.as_bytes().to_vec())
    }

    #[test]
    fn a_second_open_while_running_spawns_nothing() {
        let (mut launcher, spawner) = launcher();
        assert!(matches!(launcher.open(bytes("a")), OpenOutcome::Launched));
        assert!(launcher.is_open());
        assert!(matches!(
            launcher.open(bytes("a")),
            OpenOutcome::AlreadyOpen
        ));
        assert_eq!(spawner.spawns.get(), 1);
    }

    #[test]
    fn a_failed_spawn_leaves_the_launcher_closed_and_reopenable() {
        let (mut launcher, spawner) = launcher();
        spawner.fail.set(true);
        let OpenOutcome::Failed(err) = launcher.open(bytes("a")) else {
            panic!("the spawn should fail");
        };
        assert!(err.to_string().contains("no such program"));
        assert!(!launcher.is_open());
        // Nothing to close, so nothing to reload.
        assert!(!launcher.closed(bytes("b")));
        spawner.fail.set(false);
        assert!(matches!(launcher.open(bytes("a")), OpenOutcome::Launched));
        assert_eq!(spawner.spawns.get(), 1);
    }

    #[test]
    fn only_a_changed_file_reloads() {
        let (mut launcher, _) = launcher();
        launcher.open(bytes("same"));
        assert!(!launcher.closed(bytes("same")));
        launcher.open(bytes("before"));
        assert!(launcher.closed(bytes("after")));
        // A file created or removed while the window was open is a change.
        launcher.open(None);
        assert!(launcher.closed(bytes("new")));
        launcher.open(bytes("old"));
        assert!(launcher.closed(None));
    }

    #[test]
    fn a_file_missing_before_and_after_does_not_reload() {
        let (mut launcher, _) = launcher();
        launcher.open(None);
        assert!(!launcher.closed(None));
    }

    #[test]
    fn open_launches_again_after_a_close() {
        let (mut launcher, spawner) = launcher();
        launcher.open(bytes("a"));
        launcher.closed(bytes("a"));
        assert!(!launcher.is_open());
        assert!(matches!(launcher.open(bytes("a")), OpenOutcome::Launched));
        assert_eq!(spawner.spawns.get(), 2);
        // A second close event for one window reloads nothing.
        assert!(launcher.closed(bytes("b")));
        assert!(!launcher.closed(bytes("c")));
    }

    #[test]
    fn config_snapshot_reads_the_file_or_gives_none() {
        let dir = TempDir::new("settings-snapshot");
        let path = dir.path().join("config.toml");
        assert_eq!(config_snapshot(None), None);
        assert_eq!(config_snapshot(Some(&path)), None);
        fs::write(&path, "master_gain = 1.0\n").unwrap();
        assert_eq!(config_snapshot(Some(&path)), bytes("master_gain = 1.0\n"));
    }

    /// Runs this test binary listing no test: a harmless process that exits
    /// at once and prints nothing.
    fn quiet_child() -> Result<Command> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--list", "--exact", "no-such-test"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Ok(command)
    }

    fn missing_program() -> Result<Command> {
        Ok(Command::new("/nonexistent/sound-babound-settings-test"))
    }

    fn counting_spawner(command: fn() -> Result<Command>) -> (ProcessSpawner, Arc<Mutex<usize>>) {
        let exits = Arc::new(Mutex::new(0));
        let counter = Arc::clone(&exits);
        let spawner = ProcessSpawner {
            command,
            on_exit: Arc::new(move || *counter.lock().unwrap() += 1),
        };
        (spawner, exits)
    }

    #[test]
    fn the_process_spawner_reports_the_exit() {
        let (mut spawner, exits) = counting_spawner(quiet_child);
        spawner.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while *exits.lock().unwrap() == 0 {
            assert!(Instant::now() < deadline, "the exit was never reported");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(*exits.lock().unwrap(), 1);
    }

    #[test]
    fn a_process_that_fails_to_start_reports_no_exit() {
        let (mut spawner, exits) = counting_spawner(missing_program);
        let err = spawner.spawn().unwrap_err();
        assert!(format!("{err:#}").contains("cannot launch the settings window"));
        // The waiter holds a clone of `on_exit` until it ends: wait for that,
        // so a late call would be counted.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Arc::strong_count(&spawner.on_exit) > 1 {
            assert!(Instant::now() < deadline, "the waiter thread never ended");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*exits.lock().unwrap(), 0);
    }

    #[test]
    fn the_production_command_runs_this_executable_with_settings() {
        let command = settings_command().unwrap();
        assert_eq!(command.get_program(), std::env::current_exe().unwrap());
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["settings"]);
    }
}
