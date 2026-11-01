//! Platform-specific glue for the app.
//!
//! Each OS has its own submodule, picked by `cfg(target_os)` here; shared code
//! only calls the functions re-exported from this module.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::{
    VirtualMic, attach_parent_console, create_virtual_mic, default_virtual_device,
    global_hotkeys_supported, virtual_device_missing_hint, virtual_input_exclusions,
    virtual_mic_hint, virtual_mic_not_found,
};
#[cfg(target_os = "macos")]
pub use macos::{
    attach_parent_console, default_virtual_device, microphone_access, prepare_event_loop,
    virtual_device_missing_hint, virtual_input_exclusions, virtual_mic_hint, virtual_mic_not_found,
    with_tray_icon,
};
#[cfg(target_os = "windows")]
pub use windows::{
    attach_parent_console, default_virtual_device, virtual_device_missing_hint,
    virtual_input_exclusions, virtual_mic_hint, virtual_mic_not_found,
};

/// Fallback for every other OS: nothing to attach.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn attach_parent_console() {}

/// Fallback for every other OS: no known virtual cable, so the user names
/// the device in the config. A blank pattern matches no device.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn default_virtual_device() -> &'static str {
    ""
}

/// Fallback for every other OS: only the configured pattern is excluded.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn virtual_input_exclusions(pattern: &str) -> Vec<String> {
    vec![pattern.to_string()]
}

/// Fallback for every other OS.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn virtual_mic_hint() -> &'static str {
    "set [mic] virtual_device to a virtual audio cable's playback device"
}

/// Fallback for every other OS: the tray status when nothing matches.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn virtual_mic_not_found() -> &'static str {
    "not found, check [mic] virtual_device"
}

/// Fallback for every other OS: what `mic-check` suggests when nothing
/// matches.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn virtual_device_missing_hint() -> &'static str {
    "set [mic] virtual_device to a virtual audio cable's playback device"
}

/// Everywhere but Linux the app creates no virtual device: the user installs
/// a virtual cable. Uninhabited, so `create_virtual_mic` can only return
/// `None`.
#[cfg(not(target_os = "linux"))]
pub enum VirtualMic {}

/// Everywhere but Linux there is no virtual device to create.
#[cfg(not(target_os = "linux"))]
pub fn create_virtual_mic() -> anyhow::Result<Option<VirtualMic>> {
    Ok(None)
}

/// Whether the app may record from the microphone, as the OS reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneAccess {
    /// The user allowed it.
    Granted,
    /// The user refused it: the capture stays off.
    Denied,
    /// The user has not been asked yet; opening the capture asks.
    #[allow(dead_code)] // Only macOS reports it.
    Undetermined,
    /// The OS cannot tell (macOS before 14): the capture is tried.
    #[allow(dead_code)] // Only macOS reports it.
    Unknown,
}

impl MicrophoneAccess {
    /// Whether the capture may be opened. Only an explicit denial keeps it
    /// off; the mic sink still plays the sounds then.
    pub fn allows_capture(self) -> bool {
        self != MicrophoneAccess::Denied
    }
}

/// Everywhere but macOS no microphone permission gates the capture.
#[cfg(not(target_os = "macos"))]
pub fn microphone_access() -> MicrophoneAccess {
    MicrophoneAccess::Granted
}

/// Everywhere but macOS the tray app needs no event loop tweaks.
#[cfg(not(target_os = "macos"))]
pub fn prepare_event_loop<T: 'static>(_event_loop: &mut tao::event_loop::EventLoop<T>) {}

/// Template icons are a macOS concept. Elsewhere the icon is drawn in a
/// neutral gray that stays visible on light and dark taskbars.
#[cfg(not(target_os = "macos"))]
pub fn with_tray_icon(
    builder: tray_icon::TrayIconBuilder,
) -> anyhow::Result<tray_icon::TrayIconBuilder> {
    Ok(builder.with_icon(crate::icon::tray_icon([0x9e, 0x9e, 0x9e])?))
}

/// Global hotkeys work everywhere except Linux Wayland sessions, which only
/// the Linux module can detect.
#[cfg(not(target_os = "linux"))]
pub fn global_hotkeys_supported() -> bool {
    true
}

/// Whether the session described by `var` (an environment lookup) is a
/// Wayland session.
///
/// `XDG_SESSION_TYPE` decides when it is set to a known type; otherwise a
/// non-empty `WAYLAND_DISPLAY` means Wayland. Kept here, free of OS calls, so
/// the detection is testable on every OS.
#[cfg(any(target_os = "linux", test))]
fn is_wayland_env(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    let session = var("XDG_SESSION_TYPE").map(|v| v.to_string_lossy().trim().to_ascii_lowercase());
    match session.as_deref() {
        Some("wayland") => true,
        Some("x11") | Some("tty") => false,
        _ => var("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()),
    }
}

/// The null sink the Linux mic sink plays into.
#[cfg(any(target_os = "linux", test))]
const PACTL_SINK_NAME: &str = "sound_babound_sink";
/// The remapped source that call apps record from.
#[cfg(any(target_os = "linux", test))]
const PACTL_SOURCE_NAME: &str = "sound_babound_mic";
#[cfg(any(target_os = "linux", test))]
const PACTL_SINK_DESCRIPTION: &str = "Sound-Babound Sink";
#[cfg(any(target_os = "linux", test))]
const PACTL_SOURCE_DESCRIPTION: &str = "Sound-Babound Mic";

/// A `<kind>_properties` module argument that sets `device.description`.
///
/// The whole value is double-quoted for the module argument parser and the
/// description single-quoted for the property list parser, so the space in
/// it survives both (on PulseAudio and pipewire-pulse alike).
#[cfg(any(target_os = "linux", test))]
fn description_arg(kind: &str, description: &str) -> String {
    format!("{kind}_properties=\"device.description='{description}'\"")
}

/// `pactl` arguments that load the null sink the mic sink writes to.
#[cfg(any(target_os = "linux", test))]
fn null_sink_args() -> Vec<String> {
    vec![
        "load-module".to_string(),
        "module-null-sink".to_string(),
        format!("sink_name={PACTL_SINK_NAME}"),
        description_arg("sink", PACTL_SINK_DESCRIPTION),
    ]
}

/// `pactl` arguments that load the remap source call apps record from: the
/// null sink's monitor, under its own name and description.
#[cfg(any(target_os = "linux", test))]
fn remap_source_args() -> Vec<String> {
    vec![
        "load-module".to_string(),
        "module-remap-source".to_string(),
        format!("master={PACTL_SINK_NAME}.monitor"),
        format!("source_name={PACTL_SOURCE_NAME}"),
        description_arg("source", PACTL_SOURCE_DESCRIPTION),
    ]
}

/// `pactl` arguments that list the loaded modules, one per line.
#[cfg(any(target_os = "linux", test))]
fn list_modules_args() -> Vec<String> {
    vec![
        "list".to_string(),
        "short".to_string(),
        "modules".to_string(),
    ]
}

/// `pactl` arguments that unload module `id`.
#[cfg(any(target_os = "linux", test))]
fn unload_module_args(id: u32) -> Vec<String> {
    vec!["unload-module".to_string(), id.to_string()]
}

/// The module index `pactl load-module` prints.
#[cfg(any(target_os = "linux", test))]
fn parse_module_id(stdout: &str) -> anyhow::Result<u32> {
    let trimmed = stdout.trim();
    trimmed
        .parse()
        .map_err(|_| anyhow::anyhow!("pactl load-module printed {trimmed:?}, not a module index"))
}

/// Whether the whitespace-separated module `args` contain `key=value`, with
/// the value optionally quoted.
#[cfg(any(target_os = "linux", test))]
fn has_module_arg(args: &str, key: &str, value: &str) -> bool {
    args.split_whitespace().any(|token| {
        token
            .split_once('=')
            .is_some_and(|(k, v)| k == key && v.trim_matches(|c| c == '"' || c == '\'') == value)
    })
}

/// The modules in `pactl list short modules` output that an earlier run left
/// behind (for example after a crash), in unload order: the remap sources
/// first, since they read from the null sinks.
///
/// Each line is `index<TAB>name<TAB>arguments...`. Lines that do not parse
/// and modules this app did not create are skipped.
#[cfg(any(target_os = "linux", test))]
fn stale_modules(list: &str) -> Vec<u32> {
    let mut sources = Vec::new();
    let mut sinks = Vec::new();
    for line in list.lines() {
        let mut fields = line.split('\t');
        let (Some(id), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Ok(id) = id.trim().parse::<u32>() else {
            continue;
        };
        let args = fields.next().unwrap_or("");
        match name.trim() {
            "module-remap-source" if has_module_arg(args, "source_name", PACTL_SOURCE_NAME) => {
                sources.push(id);
            }
            "module-null-sink" if has_module_arg(args, "sink_name", PACTL_SINK_NAME) => {
                sinks.push(id);
            }
            _ => {}
        }
    }
    sources.extend(sinks);
    sources
}

/// The null sink and remap source behind the Linux virtual microphone, loaded
/// through `run` (one `pactl` invocation per call, returning its stdout).
///
/// Dropping it unloads both modules in reverse order. The runner is injected
/// so that the sequence is testable on every OS; the Linux module passes the
/// real `pactl`.
#[cfg(any(target_os = "linux", test))]
struct PactlDevice<R: FnMut(&[String]) -> anyhow::Result<String>> {
    run: R,
    sink: u32,
    source: u32,
}

#[cfg(any(target_os = "linux", test))]
impl<R: FnMut(&[String]) -> anyhow::Result<String>> PactlDevice<R> {
    /// Unloads modules left by an earlier run, then loads the null sink and
    /// the remap source. If the source fails to load, the sink is unloaded
    /// again before the error is returned.
    fn create(mut run: R) -> anyhow::Result<Self> {
        let list = run(&list_modules_args())?;
        for id in stale_modules(&list) {
            tracing::info!(module = id, "unloading a stale virtual mic module");
            run(&unload_module_args(id))?;
        }
        let sink = parse_module_id(&run(&null_sink_args())?)?;
        let source = match run(&remap_source_args()).and_then(|out| parse_module_id(&out)) {
            Ok(source) => source,
            Err(err) => {
                if let Err(unload) = run(&unload_module_args(sink)) {
                    tracing::warn!(module = sink, "could not unload the null sink: {unload:#}");
                }
                return Err(err);
            }
        };
        tracing::info!(
            sink,
            source,
            "created the virtual mic \"{PACTL_SOURCE_DESCRIPTION}\""
        );
        Ok(Self { run, sink, source })
    }
}

#[cfg(any(target_os = "linux", test))]
impl<R: FnMut(&[String]) -> anyhow::Result<String>> Drop for PactlDevice<R> {
    fn drop(&mut self) {
        for id in [self.source, self.sink] {
            if let Err(err) = (self.run)(&unload_module_args(id)) {
                tracing::warn!(
                    module = id,
                    "could not unload a virtual mic module: {err:#}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::ffi::OsString;
    use std::rc::Rc;

    use super::*;

    #[test]
    fn only_a_denial_keeps_the_capture_off() {
        assert!(MicrophoneAccess::Granted.allows_capture());
        assert!(MicrophoneAccess::Undetermined.allows_capture());
        assert!(MicrophoneAccess::Unknown.allows_capture());
        assert!(!MicrophoneAccess::Denied.allows_capture());
    }

    /// The macOS bundle's Info.plist template (`scripts/bundle-macos.sh`).
    const INFO_PLIST: &str = include_str!("../../macos/Info.plist");

    /// The element right after `<key>{key}</key>` in [`INFO_PLIST`].
    fn plist_value(key: &str) -> &'static str {
        let tag = format!("<key>{key}</key>");
        let start = INFO_PLIST.find(&tag).unwrap_or_else(|| panic!("no {key}")) + tag.len();
        let rest = INFO_PLIST[start..].trim_start();
        let end = rest.find('\n').unwrap_or(rest.len());
        rest[..end].trim_end()
    }

    #[test]
    fn info_plist_has_what_the_microphone_prompt_needs() {
        assert_eq!(
            plist_value("CFBundleExecutable"),
            format!("<string>{}</string>", env!("CARGO_BIN_NAME"))
        );
        assert_eq!(plist_value("LSUIElement"), "<true/>");
        let usage = plist_value("NSMicrophoneUsageDescription");
        assert!(usage.starts_with("<string>") && usage.ends_with("</string>"));
        assert!(usage.len() > "<string></string>".len() + 20, "{usage}");
        assert!(plist_value("CFBundleIdentifier").starts_with("<string>"));
        assert_eq!(
            plist_value("CFBundleShortVersionString"),
            "<string>@VERSION@</string>",
            "the bundle script substitutes the version"
        );
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn attach_parent_console_is_safe_to_call_twice() {
        attach_parent_console();
        attach_parent_console();
    }

    #[test]
    fn virtual_input_exclusions_start_with_the_pattern() {
        let exclusions = virtual_input_exclusions("My Cable");
        assert_eq!(exclusions.first().map(String::as_str), Some("My Cable"));
        let defaults = virtual_input_exclusions(default_virtual_device());
        assert_eq!(defaults[0], default_virtual_device());
    }

    #[test]
    fn wayland_session_type_means_wayland() {
        assert!(is_wayland_env(env(&[("XDG_SESSION_TYPE", "wayland")])));
        assert!(is_wayland_env(env(&[("XDG_SESSION_TYPE", " Wayland ")])));
        assert!(is_wayland_env(env(&[
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "wayland-0"),
        ])));
    }

    #[test]
    fn x11_session_type_wins_over_wayland_display() {
        assert!(!is_wayland_env(env(&[("XDG_SESSION_TYPE", "x11")])));
        assert!(!is_wayland_env(env(&[
            ("XDG_SESSION_TYPE", "x11"),
            ("WAYLAND_DISPLAY", "wayland-0"),
        ])));
    }

    #[test]
    fn wayland_display_decides_without_a_known_session_type() {
        assert!(is_wayland_env(env(&[("WAYLAND_DISPLAY", "wayland-0")])));
        assert!(is_wayland_env(env(&[
            ("XDG_SESSION_TYPE", "unspecified"),
            ("WAYLAND_DISPLAY", "wayland-1"),
        ])));
        assert!(!is_wayland_env(env(&[("WAYLAND_DISPLAY", "")])));
        assert!(!is_wayland_env(env(&[])));
    }

    #[test]
    fn load_module_arguments_name_the_devices() {
        assert_eq!(
            null_sink_args(),
            [
                "load-module",
                "module-null-sink",
                "sink_name=sound_babound_sink",
                "sink_properties=\"device.description='Sound-Babound Sink'\"",
            ]
        );
        assert_eq!(
            remap_source_args(),
            [
                "load-module",
                "module-remap-source",
                "master=sound_babound_sink.monitor",
                "source_name=sound_babound_mic",
                "source_properties=\"device.description='Sound-Babound Mic'\"",
            ]
        );
        assert_eq!(list_modules_args(), ["list", "short", "modules"]);
        assert_eq!(unload_module_args(42), ["unload-module", "42"]);
    }

    #[test]
    fn module_ids_parse_from_load_module_output() {
        assert_eq!(parse_module_id("536870913\n").ok(), Some(536_870_913));
        assert_eq!(parse_module_id(" 7 ").ok(), Some(7));
        assert!(parse_module_id("").is_err());
        assert!(parse_module_id("Failure: Module initialization failed").is_err());
    }

    #[test]
    fn stale_modules_are_found_remap_sources_first() {
        let list = "\
0\tmodule-device-restore\t\t
12\tmodule-null-sink\tsink_name=sound_babound_sink sink_properties=\"device.description='Sound-Babound Sink'\"\t1
13\tmodule-remap-source\tmaster=sound_babound_sink.monitor source_name=sound_babound_mic source_properties=\"device.description='Sound-Babound Mic'\"\t
20\tmodule-null-sink\tsink_name=\"sound_babound_sink\"
21\tmodule-remap-source\tsource_name='sound_babound_mic' master=sound_babound_sink.monitor
";
        assert_eq!(stale_modules(list), [13, 21, 12, 20]);
    }

    #[test]
    fn foreign_modules_are_not_stale() {
        let list = "\
1\tmodule-null-sink\tsink_name=other_sink
2\tmodule-null-sink\tsink_name=sound_babound_sink2
3\tmodule-remap-source\tmaster=sound_babound_sink.monitor source_name=someone_else
4\tmodule-loopback\tsink=sound_babound_sink
5\tmodule-null-sink\t
6\tmodule-remap-sink\tsink_name=sound_babound_sink
";
        assert!(stale_modules(list).is_empty());
    }

    #[test]
    fn malformed_module_lines_are_skipped() {
        let list = "\
garbage
\t\t
x\tmodule-null-sink\tsink_name=sound_babound_sink
-1\tmodule-null-sink\tsink_name=sound_babound_sink
9 module-null-sink sink_name=sound_babound_sink

7\tmodule-null-sink\tsink_name=sound_babound_sink
";
        assert_eq!(stale_modules(list), [7]);
        assert!(stale_modules("").is_empty());
    }

    /// A fake `pactl`: records every invocation and answers from `respond`.
    fn fake_pactl(
        calls: &Rc<RefCell<Vec<Vec<String>>>>,
        respond: impl Fn(&[String]) -> anyhow::Result<String>,
    ) -> impl FnMut(&[String]) -> anyhow::Result<String> {
        let calls = Rc::clone(calls);
        move |args| {
            calls.borrow_mut().push(args.to_vec());
            respond(args)
        }
    }

    fn healthy_pactl(list: &'static str) -> impl Fn(&[String]) -> anyhow::Result<String> {
        move |args| match args[0].as_str() {
            "list" => Ok(list.to_string()),
            "load-module" if args[1] == "module-null-sink" => Ok("31\n".to_string()),
            "load-module" => Ok("32\n".to_string()),
            _ => Ok(String::new()),
        }
    }

    #[test]
    fn create_unloads_stale_modules_then_loads_sink_then_source() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let list = "\
4\tmodule-null-sink\tsink_name=sound_babound_sink
5\tmodule-remap-source\tsource_name=sound_babound_mic
";
        let device = PactlDevice::create(fake_pactl(&calls, healthy_pactl(list)));
        let device = device.ok();
        assert_eq!(device.as_ref().map(|d| (d.sink, d.source)), Some((31, 32)));
        assert_eq!(
            *calls.borrow(),
            [
                list_modules_args(),
                unload_module_args(5),
                unload_module_args(4),
                null_sink_args(),
                remap_source_args(),
            ]
        );
        calls.borrow_mut().clear();
        drop(device);
        assert_eq!(
            *calls.borrow(),
            [unload_module_args(32), unload_module_args(31)],
            "the source unloads before the sink it reads from"
        );
    }

    #[test]
    fn drop_tries_the_sink_even_when_the_source_unload_fails() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let healthy = healthy_pactl("");
        let device = PactlDevice::create(fake_pactl(&calls, move |args| {
            if args == unload_module_args(32).as_slice() {
                anyhow::bail!("no such module");
            }
            healthy(args)
        }));
        assert!(device.is_ok());
        calls.borrow_mut().clear();
        drop(device);
        assert_eq!(
            *calls.borrow(),
            [unload_module_args(32), unload_module_args(31)]
        );
    }

    #[test]
    fn a_failed_source_load_unloads_the_sink() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let healthy = healthy_pactl("");
        let result = PactlDevice::create(fake_pactl(&calls, move |args| {
            if args == remap_source_args().as_slice() {
                anyhow::bail!("pactl exited with status 1: Module initialization failed");
            }
            healthy(args)
        }));
        let err = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("Module initialization failed"), "{err}");
        assert_eq!(
            *calls.borrow(),
            [
                list_modules_args(),
                null_sink_args(),
                remap_source_args(),
                unload_module_args(31),
            ]
        );
    }

    #[test]
    fn a_failing_pactl_is_an_error_before_anything_loads() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let result = PactlDevice::create(fake_pactl(&calls, |_| anyhow::bail!("pactl not found")));
        assert!(result.is_err());
        assert_eq!(*calls.borrow(), [list_modules_args()]);
    }

    #[test]
    fn a_failed_stale_unload_is_an_error() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let healthy = healthy_pactl("8\tmodule-null-sink\tsink_name=sound_babound_sink\n");
        let result = PactlDevice::create(fake_pactl(&calls, move |args| {
            if args[0] == "unload-module" {
                anyhow::bail!("pactl exited with status 1");
            }
            healthy(args)
        }));
        assert!(result.is_err());
        assert_eq!(
            *calls.borrow(),
            [list_modules_args(), unload_module_args(8)]
        );
    }

    #[test]
    fn a_garbled_sink_index_is_an_error() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let result = PactlDevice::create(fake_pactl(&calls, |args| match args[0].as_str() {
            "list" => Ok(String::new()),
            _ => Ok("oops".to_string()),
        }));
        assert!(result.is_err());
        assert_eq!(*calls.borrow(), [list_modules_args(), null_sink_args()]);
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn create_virtual_mic_is_none_off_linux() {
        assert!(create_virtual_mic().is_ok_and(|mic| mic.is_none()));
    }
}
