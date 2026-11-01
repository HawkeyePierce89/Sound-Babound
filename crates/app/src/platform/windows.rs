//! Windows glue.

use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

/// Attaches to the console of the parent process, if it has one.
///
/// The binary is built for the GUI subsystem so tray mode opens no console
/// window. In CLI mode this makes stdout, stderr and log output visible in the
/// terminal the app was started from. Failure (no parent console, or already
/// attached) is harmless and ignored.
pub fn attach_parent_console() {
    // SAFETY: AttachConsole takes a process id by value and has no memory
    // safety preconditions.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}
