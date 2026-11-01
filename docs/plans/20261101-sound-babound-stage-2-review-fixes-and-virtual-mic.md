# Sound-Babound stage 2: review fixes and the virtual microphone

## Overview

Two parts.

Part A fixes the four findings from the stage 1 acceptance review:

1. A startup grace for the stall watchdog.
2. A tray that starts even when the config file is malformed.
3. Plays accepted during a live rebuild.
4. Expected device-changed stream errors logged at debug instead of warn.

Part B adds the virtual microphone:

- A second output path, the mic sink. It targets a virtual cable's playback side, chosen by name match.
- One capture stream that follows the default input and refuses to capture the virtual device itself.
- A drift-compensated lock-free passthrough from the capture into the mic sink's mixer.
- Fan-out of every sound command to both mixers.

Each of the three streams (monitor, mic sink, capture) runs under its own instance of the same generic supervisor; only the factory differs. On Linux the app creates the virtual device itself through `pactl`. On macOS it gets a minimal app bundle so that microphone permission works. The app adds a `[mic]` config section, tray status and toggle, and a `mic-check` CLI. Mic is off by default, and with it off the app behaves exactly as in stage 1.

## Context

- Engine files involved: `crates/engine/src/{supervisor.rs, sink.rs, cpal_sink.rs, mixer.rs, convert.rs, engine.rs, error.rs, lib.rs, test_support.rs, watcher/mod.rs}`.
- App files involved: `crates/app/src/{config.rs, actions.rs, tray_app.rs, cli.rs, hotkeys.rs, platform/*.rs}`, plus `README.md` and `CLAUDE.md`.
- New files:
  - `crates/engine/src/passthrough.rs`: ring buffer, drift controller and resampler.
  - `crates/engine/src/capture.rs`: cpal capture factory and input selection.
  - `crates/engine/src/platform/{mod,macos,windows,linux}.rs`: cpal host choice for the mic paths. On Linux this is one process-wide PulseAudio host (`platform/shared_host.rs`, added in review), falling back to ALSA by id.
  - `crates/engine/src/watcher/{poll,inputs}.rs` (added in review): the polling watcher and the Linux capture's input query.
  - `crates/app/macos/Info.plist`
  - `scripts/bundle-macos.sh`
- Patterns to reuse:
  - The pure `SupervisorCore` with a fake clock.
  - `FakeSinkFactory`/`FakeWatcher` and `wait_until`.
  - The `Arc<Mutex<Mixer>>` handoff invariant.
  - The pending-release and garbage queue rule.
  - `#[cfg(any(target_os = "linux", test))]` pure helpers in `platform/mod.rs`, like `is_wayland_env`.
  - The recording `AudioControl` and fake `HotkeyBackend` in app tests.
- Dependencies:
  - cpal 0.18 with the `pulseaudio` feature, Linux target only. It is a pure-Rust protocol client, needs no libpulse or libclang, and also works against pipewire-pulse.
  - `toml_edit` in the app, so the toggle can be saved without losing the user's comments.
  - `objc2-avf-audio` (`AVAudioApplication`) in the app, macOS only, to read the microphone permission.
  - The passthrough uses the existing `crossbeam-queue` `ArrayQueue<f32>`, so no new ring-buffer crate is needed and the "only ArrayQueue push in realtime code" rule holds.
- Decisions:
  - Resampling: a hand-written linear interpolator, the same kind the mixer already uses, with a continuously adjustable ratio. It allocates nothing. rubato is not used.
  - Watchers: every supervisor starts its own platform watcher instance (three CoreAudio listeners, three IMMNotificationClients; on Linux, from review on, `platform_watcher(role)` gives the capture an input poller and the outputs no-op watchers). This keeps the supervisor symmetric with no fan-out plumbing. Each instance filters events through a per-role rebuild policy.
  - Linux virtual device: `pactl` command-line tools. They work on both PulseAudio and PipeWire (pipewire-pulse) through one code path, need no native bindings, and keep the Linux cross-check working with the stub pkg-config.
    - `module-null-sink sink_name=sound_babound_sink`, which the mic sink writes to.
    - `module-remap-source master=sound_babound_sink.monitor source_name=sound_babound_mic`, with the description "Sound-Babound Mic".
  - The passthrough signal is mono: the capture downmixes by averaging. The mic mixer maps it to output channels with the same mapping used for mono sounds.
  - Device name match: a case-insensitive substring of the device's display name or its id. The first match in enumeration order wins.

## Development Approach

- **Testing approach**: Regular (code first, then tests in the same task).
- Complete each task fully before moving to the next.
- All stage 1 conventions from CLAUDE.md apply:
  - The realtime rules (no allocation or freeing, no blocking lock, no I/O, no tracing; only `ArrayQueue` pushes).
  - The mixer handoff invariant and the pending-release rule.
  - Platform code only in the per-OS modules. This plan adds `crates/engine/src/platform/` to the allowed locations.
  - Typed `thiserror` errors in the engine, `anyhow` in the app.
  - No `unwrap`/`expect` outside tests.
  - Fakes for every test.
- The capture callback and the mic sink callback follow the same realtime rules as `Mixer::render`.
- **Agents must never play audio through a real device, change the system default input or output, or change the system volume.**
  - Real-device checks are `#[ignore]` tests that only a human runs.
  - The demo `play` run, `mic-check` against real devices and creating the Linux virtual device are not part of any automated step.
- After every task, all of these must be clean:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets` (zero warnings)
  - `cargo test --workspace`
  - `cargo check --workspace --target x86_64-pc-windows-msvc`
- Tasks that touch Linux code also run the stub-pkg-config Linux clippy from CLAUDE.md.
- **CRITICAL: every task MUST include new/updated tests**
- **CRITICAL: all tests must pass before starting next task**

## Implementation Steps

### Task 1: Stall watchdog startup grace (Part A.1)

**Files:**

- Modify: `crates/engine/src/supervisor.rs`

- [x] Add `startup_grace: Duration` (default 10 s) to `Timings`.
- [x] `on_build_result` takes the heartbeat value read just before `build`, as the baseline.
- [x] `ActiveStream` records `built_at` and whether the heartbeat has moved past the baseline.
- [x] Until the first change past the baseline, the watchdog deadline is `built_at + startup_grace`. After that it is `last_beat_at + stall`. `next_deadline` returns the same deadlines.
- [x] Worker: read the heartbeat before calling `build` and pass it on.
- [x] Update `heartbeat_that_never_moves_after_build_rebuilds` to the 10 s grace.
- [x] Add fake-clock tests:
  - a first callback at 5 s does not rebuild;
  - no callback for 10 s rebuilds;
  - a running stream whose heartbeat then stops for 2 s still rebuilds;
  - a callback that lands between `build` and the first sample counts as started.
- [x] Run the verification gates. All must pass before Task 2.

### Task 2: Device-changed stream errors at debug (Part A.4)

**Files:**

- Modify: `crates/engine/src/supervisor.rs`, `crates/engine/src/cpal_sink.rs`, `crates/engine/src/test_support.rs`

- [x] Replace `SupervisorMsg::StreamError(String)` with `StreamError { kind: StreamErrorKind, message: String }`, where `StreamErrorKind` is `DeviceChanged` or `Other`.
- [x] Map cpal `ErrorKind::DeviceChanged` to `DeviceChanged` in the cpal error callback. `needs_rebuild` is unchanged.
- [x] Add a pure `stream_error_level(kind) -> tracing::Level`: DEBUG for `DeviceChanged`, WARN otherwise. The worker logs at that level. Both kinds still rebuild.
- [x] Tests:
  - the level mapping;
  - the cpal `ErrorKind` to `StreamErrorKind` mapping;
  - a thread-level test where a `DeviceChanged` error still rebuilds.
- [x] Run the verification gates. All must pass before Task 3.

### Task 3: Plays accepted during a live rebuild (Part A.3)

**Files:**

- Modify: `crates/engine/src/mixer.rs`, `crates/engine/src/supervisor.rs`, `crates/engine/src/engine.rs`, `crates/engine/src/test_support.rs`

- [x] Add `play_grace: Duration` (default 1 s) to `Timings`.
- [x] `MixerShared` gets a grace deadline: an atomic count of nanoseconds since an epoch `Instant` it owns, 0 meaning none. It also gets `accepts_plays(now)`, true when `output_active || now < grace_until`.
- [x] Worker: when it drops a live sink for a rebuild, set `grace_until = now + play_grace`. A teardown with no stream never sets it.
- [x] While the grace window is open and there is no stream, skip `apply_offline` and the purge before a rebuild from offline. Queued commands keep their order and the replacement stream applies them.
- [x] Once the window expires with no stream, the existing offline drop applies.
- [x] `Engine::play` checks `accepts_plays(Instant::now())`. Update the docs on `Engine::play`, the mixer module and the supervisor module.
- [x] `FakeSinkFactory`: add a build gate (`hold_builds`, `release_builds`, `building()`) so a test can play while a build is in progress.
- [x] Add an engine test hook that takes `Timings`.
- [x] Tests:
  - live stream, then `DefaultOutputChanged`, then a play while the replacement build is held: accepted; after release, the new mixer renders it.
  - live stream, then builds that fail forever (short `play_grace`): a play inside the window is accepted and later dropped (`handle_count` returns to 1); a play after the window is rejected.
  - `play_without_output_is_rejected` still holds: the initial start has no grace.
- [x] Run the verification gates. All must pass before Task 4.

### Task 4: Malformed config at startup falls back (Part A.2)

**Files:**

- Modify: `crates/app/src/actions.rs`, `crates/app/src/tray_app.rs`, `README.md`

- [x] `startup_board`: on a parse error, log the error at error level with the path, and return `fallback_board()`.
- [x] `TrayApp` keeps `config_path`, so "Reload config" and "Open config folder" work and a fixed file can be reloaded.
- [x] Reload behaviour (keep the old config on error) is unchanged.
- [x] Replace `startup_rejects_a_malformed_config` with `startup_falls_back_on_malformed_config`, which asserts the fallback board's sounds and gain.
- [x] Add a test that a later reload of the fixed file installs it.
- [x] README: describe the startup fallback and where to find the log.
- [x] Run the verification gates. All must pass before Task 5.

### Task 5: Generic stream supervisor with roles

**Files:**

- Modify: `crates/engine/src/sink.rs`, `crates/engine/src/supervisor.rs`, `crates/engine/src/watcher/mod.rs`, `crates/engine/src/cpal_sink.rs`, `crates/engine/src/engine.rs`, `crates/engine/src/test_support.rs`, `crates/engine/src/lib.rs`

- [x] Introduce `StreamRole { Monitor, MicSink, Capture }`.
- [x] `DeviceEvent::rebuild_policy(role)` gives each role its own policy table:
  - Monitor: unchanged from stage 1.
  - MicSink: default output or input changes are ignored; list, state and removed events are identity-checked.
  - Capture: `DefaultInputChanged` always rebuilds; `DefaultOutputChanged` is ignored; list, state and removed events are identity-checked.
- [x] Rename `IfDefaultOutputChanged` to `IfIdentityChanged`.
- [x] Update `PLATFORM_EVENT_TABLE` to carry a policy per role.
- [x] Split the factory side from the realtime state:
  - `StreamFactory` (renamed from `SinkFactory`): `role()`, `build(target, on_error)`, `current_identity()` (the identity a build would pick now).
  - `Endpoint`: the state the supervisor watches, with `target()`, `heartbeat()`, `set_active()`, and housekeeping while there is no stream (offline play handling with the Task 3 grace, and garbage draining).
  - `MixerEndpoint` serves the two output roles; Task 6 adds the capture endpoint.
- [x] `spawn_supervisor<F, E, W>` is the single generic supervisor, and each instance starts its own watcher.
- [x] Build-failure log throttling for every role:
  - warn on the first failure of a streak and when the error changes;
  - debug for repeats;
  - info when a build succeeds after failures.
  - It is a small pure `FailureLog` in `supervisor.rs`.
- [x] Each supervisor publishes a `StreamStatus` for UI reads: Starting, Running(`StreamInfo`) or Failed(error text and kind). It sits in a control-thread-only `Arc<Mutex<..>>`, never touched by a callback.
- [x] `FakeWatcher` broadcasts to every started supervisor. `FakeSinkFactory` takes a role and a target.
- [x] Keep every stage 1 supervisor and engine test passing with the monitor role.
- [x] Tests:
  - the role policy tables;
  - `FailureLog` sequences;
  - status transitions;
  - one fake watcher feeding two supervisors.
- [x] Run the verification gates. All must pass before Task 6.

### Task 6: Passthrough ring, drift controller and resampler

**Files:**

- Create: `crates/engine/src/passthrough.rs`
- Modify: `crates/engine/src/convert.rs`, `crates/engine/src/lib.rs`

- [x] `PassthroughShared`:
  - an `ArrayQueue<f32>` of mono samples, allocated once with room for 0.5 s at 192 kHz;
  - an atomic capture rate and a generation counter;
  - a capture heartbeat and an active flag;
  - underrun and overrun counters.
- [x] Writer (capture side, realtime-safe):
  - downmix interleaved input to mono, then `force_push`;
  - count an overrun when the oldest sample is displaced, or when the fill passes the high mark (target + 50 ms) and the oldest samples are popped;
  - bump the heartbeat.
- [x] `begin_capture(rate)`, called from a capture build before its stream starts: empty the ring, store the rate, bump the generation.
- [x] `PassthroughReader` (mic sink side, owned by the mic `Mixer`, realtime-safe):
  - Priming: output silence until the fill reaches the target (30 ms of capture-rate frames).
  - Resampling: linear interpolation with ratio `capture_rate / sink_rate * (1 + correction)`.
  - Correction: a PI controller on an EMA-smoothed fill error, clamped to ±0.2 %.
  - Underrun: when the ring runs empty mid-block, count it, output silence for the rest of the block and re-prime.
  - On a generation or rate change, reset and re-prime.
- [x] `convert.rs`: add an `InputSample` trait (device format to f32) for every cpal PCM format, including I24 and U24.
- [x] Drift tests that simulate device clocks, with no threads and no devices:
  - nominal 48000/48000 with the sink clock at 48000.5 Hz for 10 simulated minutes, mismatched block cadence (capture 480-frame blocks, sink 512): after 5 s of settling, the fill stays within target ± 20 ms with zero underruns and zero overruns;
  - the same for 44100 to 48000, and for ±200 ppm drift in both directions.
- [x] Unit tests:
  - underrun gives silence and re-primes;
  - overrun drops the oldest audio;
  - a generation change resets;
  - downmixing;
  - input conversion at full scale.
- [x] Run the verification gates. All must pass before Task 7.

### Task 7: Mic mixer: passthrough input and path gains

**Files:**

- Modify: `crates/engine/src/mixer.rs`

- [x] `Mixer` gets an optional `PassthroughReader`. The monitor never has one, so it can never carry the microphone.
- [x] `configure(rate, channels)` passes the sink rate to the reader.
- [x] In `mix`, add `input_gain × passthrough` before the existing clamp, using the mono-sound channel mapping.
- [x] Add `Command::SetPathGains { sound, input }`. The mixer applies `voice gain × master × sound path gain` and the input gain, all sanitized like the existing gains. The defaults are 1.0, so the monitor is unchanged.
- [x] Tests:
  - passthrough is mixed and clamped;
  - path gains multiply;
  - a mixer without a reader is identical to stage 1, checked against the existing render tests;
  - rendering with a reader allocates nothing, using the existing patterns: no new Vec, and `render_scratch` matches `render`.
- [x] Run the verification gates. All must pass before Task 8.

### Task 8: cpal mic sink and capture factories with feedback exclusion

**Files:**

- Create: `crates/engine/src/capture.rs`, `crates/engine/src/platform/{mod,macos,windows,linux}.rs`
- Modify: `crates/engine/src/cpal_sink.rs`, `crates/engine/src/error.rs`, `crates/engine/Cargo.toml`, `crates/engine/src/lib.rs`

- [x] `platform::mic_host()`: the default host on macOS and Windows; the PulseAudio host on Linux, falling back to the default host (changed in review: one shared PulseAudio host, falling back to ALSA by id). Selection happens only in `platform/mod.rs`.
- [x] Enable cpal's `pulseaudio` feature for the Linux target only.
- [x] Generalize `CpalSinkFactory` into an output factory with a target:
  - `Default` (the monitor, as in stage 1);
  - `Named(pattern)` (the mic sink, on `mic_host()`). It re-enumerates output devices on every build and every identity query and never caches.
- [x] `BuildError` gains `DeviceNotFound(String)` and `FeedbackExcluded(String)`.
- [x] A pure `choose_input(default, available, last_physical, exclusions) -> InputChoice`:
  - a default input matching an exclusion (the virtual device or its monitor) falls back to the most recent physical input if it is still present;
  - otherwise it returns `Excluded`.
  - The factory remembers the last non-excluded default input identity.
- [x] `CpalCaptureFactory`:
  - re-queries the default input on every build;
  - applies `choose_input`;
  - logs the fallback, or the exclusion, at warn once per change;
  - `Excluded` becomes `BuildError::FeedbackExcluded`, so the supervisor retries and the Task 5 throttle applies;
  - calls `begin_capture(rate)`, then builds a typed input stream for every PCM format that writes through the passthrough writer.
- [x] `CaptureEndpoint` around `PassthroughShared`.
- [x] `probe()` on both factories: resolve the device and its default config without building a stream, for `mic-check`.
- [x] Unit tests:
  - name matching (case, display name versus id, first match);
  - `choose_input` with fake identities: default is physical; default is the virtual device with a known physical input; default is the virtual monitor with no physical input known; the last physical input is gone;
  - the warn-once decision.
- [x] Extend the `#[ignore]` smoke test. When the `BABOUND_MIC_DEVICE` env var or the default pattern resolves, it builds monitor, mic sink and capture streams and checks their `StreamInfo`. It plays nothing. (Extended; not run here, it needs real devices and is human-run.)
- [x] Run the verification gates plus the Linux stub clippy. All must pass before Task 9.

### Task 9: Engine mic path API and fan-out

**Files:**

- Modify: `crates/engine/src/engine.rs`, `crates/engine/src/lib.rs`, `crates/engine/src/test_support.rs`

- [x] `MicSettings { device_match, excluded_inputs, sound_gain, mic_gain, capture: bool }`.
- [x] `Engine::set_mic(Option<MicSettings>)`:
  - `None` stops and joins the mic sink and capture supervisors;
  - `Some` starts them;
  - a change to gains only sends `SetPathGains` with no restart;
  - any other change restarts only the mic path;
  - the monitor supervisor is never touched.
- [x] Fan-out:
  - `play` pushes to each path that `accepts_plays`, and returns true if any path accepted;
  - `stop_all` and `set_master_gain` go to both;
  - the mic path drops its own rejected commands on the caller's thread.
- [x] Accessors:
  - `Engine::mic_status()` returns `MicStatus { Off | Starting | NotFound(pattern) | Active { device, capture: CaptureStatus } }`, with `CaptureStatus` one of Running(name), Disabled or Unavailable(reason);
    (Implemented with two extra variants: `MicStatus::Failed(error)` for a mic sink build failure other than not-found, and `CaptureStatus::Starting` while the capture builds.)
  - `has_mic_output()`;
  - `active_voices()` and `output_latency()` cover both paths (sum and maximum).
- [x] Add `pub fn probe_mic(&MicSettings) -> MicProbe`.
- [x] Add a `Backends` abstraction that supplies the three factories and the watcher: `CpalBackends` for production, `FakeBackends` for tests.
- [x] Re-export only what the app needs.
- [x] Supervisor tests with fakes:
  - switching the default input rebuilds only the capture;
  - switching the default output rebuilds only the monitor;
  - neither interrupts the other (build counts and live sinks per role);
  - a mic sink that is not found retries, with only the first failure at warn (through `FailureLog`), while the monitor plays.
- [x] Engine tests with fakes:
  - with mic off, no mic or capture factory is ever built and only one supervisor and watcher run;
  - set_mic on/off/gain-only/device-change;
  - a play reaches both mixers;
  - the passthrough reaches the mic mixer but not the monitor.
- [x] Run the verification gates. All must pass before Task 10.

### Task 10: `[mic]` config section and persistence

**Files:**

- Modify: `crates/app/src/config.rs`, `crates/app/src/hotkeys.rs`, `crates/app/src/actions.rs`, `crates/app/src/platform/{mod,macos,windows,linux}.rs`, `crates/app/Cargo.toml`, `Cargo.toml`

- [x] `RawMic { enabled = false, virtual_device: Option<String>, sound_gain = 1.0, mic_gain = 1.0 }` under `[mic]`.
- [x] Resolve it into `MicConfig`: gains clamped like the existing gains (with a warning); an empty `virtual_device` falls back to the default with a warning.
- [x] Per-OS defaults in `platform`:
  - `default_virtual_device()`: "BlackHole" on macOS, "CABLE Input" on Windows, "sound_babound_sink" on Linux;
  - `virtual_input_exclusions(pattern)`: the pattern itself plus "CABLE Output" on Windows (and, for a custom cable, the pattern with its last "input" changed to "Output"), and the pattern plus "sound_babound_mic" on Linux;
  - `virtual_mic_hint()`.
- [x] Add a commented `[mic]` block to `DEFAULT_CONFIG`.
- [x] `config::set_mic_enabled(path, bool)` uses `toml_edit`. It keeps comments, creates `[mic]` if missing and writes atomically (temp file and rename).
- [x] `AudioControl` gains `set_mic`. `apply_config` sends it, built from `MicConfig`, the platform exclusions and a capture flag.
  (`set_mic` takes `&mut self`, like `Engine::set_mic`, so `apply_config` and `reload` take `&mut impl AudioControl` and a `capture: bool`; the tray passes `true` until Task 12 reads the permission. `virtual_mic_hint` and `set_mic_enabled` carry a dead-code allow until Task 13 uses them, and the hint is not yet re-exported from `platform`.)
- [x] Tests:
  - parse defaults with no `[mic]`;
  - clamping;
  - the per-OS default;
  - `set_mic_enabled` keeps comments and other keys, adds the table, and fails cleanly on a malformed file;
  - `apply_config` sends the mic settings to the recording fake.
- [x] Run the verification gates plus the Linux stub clippy. All must pass before Task 11.

### Task 11: Linux virtual device through pactl

**Files:**

- Modify: `crates/app/src/platform/{mod,linux}.rs`

- [x] Pure helpers in `platform/mod.rs`, gated `cfg(any(target_os = "linux", test))`:
  - the `load-module` argument lists for the null sink and the remap source (description "Sound-Babound Mic");
  - a parser for `pactl list short modules` that finds stale modules from a crashed run.
- [x] `linux.rs`: `VirtualMic::create()`:
  - unload stale modules;
  - load the null sink and then the remap source, keeping the module ids from stdout;
  - return a guard that unloads both in reverse order on drop.
- [x] A missing `pactl` or a non-zero exit is an error.
- [x] Other OSes: `create_virtual_mic()` returns `Ok(None)` through `platform/mod.rs`.
- [x] The app logs a failure and runs without the mic path. Wiring happens in Task 13.
  (`VirtualMic`, `create_virtual_mic` and their re-export carry a dead-code allow until Task 13 calls them. A failed stale unload is an error too; a failed remap-source load unloads the null sink before returning the error. The null sink gets the description "Sound-Babound Sink"; the mic sink still finds it by its id, `sound_babound_sink`.)
- [x] Tests (run on every OS through the `test` gate): argument construction, module-list parsing (stale, foreign and malformed lines), unload order.
- [x] Run the verification gates plus the Linux stub clippy. All must pass before Task 12.

### Task 12: macOS microphone permission and app bundle

**Files:**

- Create: `crates/app/macos/Info.plist`, `scripts/bundle-macos.sh`
- Modify: `crates/app/src/platform/{mod,macos}.rs`, `crates/app/Cargo.toml`

- [x] `platform::microphone_access()` returns `Granted`, `Denied`, `Undetermined` or `Unknown`:
  - macOS: `AVAudioApplication.recordPermission` on macOS 14 and later, `Unknown` before that;
  - other OSes: `Granted`.
- [x] `Denied` means `MicSettings.capture = false`. Log it once at warn; the mic sink still plays sounds.
- [x] `Undetermined` opens the capture, and macOS prompts.
- [x] `Info.plist`:
  - `CFBundleIdentifier`, `CFBundleExecutable sound-babound`, the version;
  - `LSUIElement true`;
  - `NSMicrophoneUsageDescription`.
- [x] `scripts/bundle-macos.sh`:
  - `cargo build --release -p sound-babound`;
  - assemble `target/release/Sound-Babound.app` (Contents/MacOS, Info.plist with the version substituted);
  - ad-hoc `codesign --sign -`, so TCC binds the plist. This is not Developer ID signing.
  - `plutil -lint`.
- [x] Test: an app unit test (`include_str!`) asserts the plist carries `NSMicrophoneUsageDescription`, `LSUIElement` and the executable name.
- [x] Run `scripts/bundle-macos.sh` and check the bundle layout. This builds only; it opens no audio.
- [x] Run the verification gates. All must pass before Task 13.
  (`actions::CaptureGate` turns the permission into the capture flag and warns once per change, only while the mic is enabled; `reload` takes the capture decision as a closure over the new `MicConfig`, so a reload that enables the mic is checked too. The bundle id is `io.github.hawkeyepierce89.sound-babound`. The `Undetermined` and `Unknown` variants carry a dead-code allow, because only macOS constructs them.)

### Task 13: Tray mic status, toggle and reload

**Files:**

- Modify: `crates/app/src/tray_app.rs`, `crates/app/src/actions.rs`

- [x] Menu: a disabled status item and a "Mic to call: on/off" item (`MENU_TOGGLE_MIC`, `Action::ToggleMic`).
- [x] A pure `mic_status_label(&MicStatus, hint)`:
  - "Virtual mic: off";
  - "Virtual mic: starting";
  - "Virtual mic: <device> (active)", with the capture state appended when the microphone is unavailable or denied;
  - "Virtual mic: not found, install BlackHole" or "install VB-CABLE", and on Linux "could not create it, see log".
- [x] While the mic is enabled, the event loop wakes every 1 s (`ControlFlow::WaitUntil`) and updates the status text only when it changed. With the mic off it stays on `Wait`.
- [x] Toggle:
  - flip the enabled flag;
  - persist it with `set_mic_enabled` (a persist failure is logged, the runtime change still applies);
  - on Linux, create or drop the `VirtualMic` guard;
  - check microphone access;
  - call `engine.set_mic`;
  - update both labels.
- [x] Startup: if the mic is enabled, create the Linux device before `apply_config`.
- [x] Reload applies `[mic]` through `apply_config`, so the monitor is not interrupted (Task 9 diff logic).
- [x] Quit drops the engine first, then the `VirtualMic` guard.
- [x] Tests:
  - every label variant;
  - the toggle action with the recording `AudioControl` and a TempDir config (persisted and applied, and applied even when persisting fails);
  - reload with changed `[mic]` sends new settings while hotkeys and sounds are unchanged.
- [x] Run the verification gates. All must pass before Task 14.
  (The Linux device follows the mic path through `actions::WithVirtualMic`, an `AudioControl` wrapper: `set_mic(Some)` creates it first, `set_mic(None)` drops it after the engine stops, so startup, reload and the toggle all keep it in step. A device that cannot be created is logged and the mic sink reports not found, shown as the Linux "could not create it, see log". The not-found text comes from a new per-OS `platform::virtual_mic_not_found()`; `MicStatus::Failed` shows "Virtual mic: error, see log", and `CaptureStatus::Disabled` shows as "microphone access denied". The long `virtual_mic_hint()` is logged when the status turns active.)

### Task 14: CLI `mic-check` and mic-aware `play`

**Files:**

- Modify: `crates/app/src/cli.rs`, `crates/app/src/main.rs`

- [x] `mic-check`:
  - reads `[mic]` from the config file without creating it; a missing or malformed file means defaults, with a note;
  - calls `probe_mic`;
  - reports:
    - whether the mic is enabled;
    - the virtual device pattern and the matched device with its format;
    - the input device the capture would use and its format, including a fallback from an excluded default;
    - the macOS microphone permission;
    - why the path is unavailable;
  - exits non-zero when the mic path is unavailable;
  - opens no stream and plays nothing.
- [x] `play`: when `[mic]` is enabled, `set_mic` before playing.
  - Wait for the monitor as before, and for the mic sink up to a short timeout; if the mic sink does not come up, warn and play on the monitor only.
  - Wait until both paths are idle, then drain for the larger output latency.
  - `play` does not create the Linux device.
- [x] Tests:
  - clap parsing of `mic-check`;
  - a pure report formatter fed fabricated `MicProbe` values (found, not found, feedback fallback, excluded with no fallback, permission denied);
  - the play wait logic with closures covering the mic timeout.
- [x] Run the verification gates. All must pass before Task 15.
  (`MicProbe` gained `excluded_default`, the default input's name when an exclusion names it, so the report can show a fallback or a refusal; a fresh probe knows no earlier physical input, so it refuses, as the tray's first capture build does. `DeviceIdentity` is re-exported so app tests can fabricate `StreamInfo`. A per-OS `platform::virtual_device_missing_hint()` gives the not-found advice; on Linux it explains that only the tray creates the device. The exit code is non-zero when the virtual device is missing, the capture cannot open or the permission is denied; the report then says "unavailable" or "sounds only". `play` starts the mic path with capture off, so it never opens the microphone, and waits for `commands_applied` to grow by one per running path. `mic-check` against real devices was not run: it is human-run.)

### Task 15: Verify acceptance criteria

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --workspace --all-targets`, with zero warnings
- [x] `cargo test --workspace`
- [x] `cargo check --workspace --target x86_64-pc-windows-msvc`
- [x] Linux stub pkg-config `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu`
- [x] Confirm each Part A test fails without its fix: revert each fix locally, run the test, restore.
- [x] Confirm the drift test (AC4), the feedback exclusion test (AC5), the role isolation tests (AC6) and the mic-off test (AC8) exist and pass.
- [x] Check test coverage of the new engine modules (passthrough, capture selection, supervisor roles); aim for 80% or more.
  (All five gates are clean. Part A reverts, each restored afterwards: A.1, an always-`stall` deadline fails the startup grace tests; A.2, returning the parse error fails `startup_falls_back_on_malformed_config` and the reload test; A.3, `accepts_plays` without the grace fails the live-rebuild engine and supervisor tests, and an unconditional offline purge fails the purge and in-window drop tests; A.4, mapping `DeviceChanged` to `Other` and warn fails `device_changed_maps_to_its_own_kind` and `device_changed_logs_at_debug_and_others_at_warn`. AC4: `passthrough::tests::drift_*` and `capture_clock_drift_holds_the_target`; AC5: the `capture::tests` `choose_input` tests; AC6: `switching_the_default_input_rebuilds_only_the_capture`, `switching_the_default_output_rebuilds_only_the_monitor`, `a_missing_mic_sink_retries_while_the_monitor_plays`; AC8: `mic_off_builds_no_mic_streams`. Coverage, measured with `-C instrument-coverage` and Xcode's llvm-cov: passthrough.rs 98% of lines, supervisor.rs 99%, sink.rs 98%; capture.rs 44% overall, but `choose_input`, `is_excluded` and `NoticeLog` are 100%, and the misses are the cpal device code (`select`, `build`, `probe`, `build_typed`) that only the `#[ignore]` smoke test reaches.)

### Task 16: Update documentation

- [x] README:
  - a "Virtual microphone" setup guide per OS:
    - macOS: install BlackHole, noting that it is GPL and installed separately; build the bundle with `scripts/bundle-macos.sh`; the permission prompt and how to re-enable it in System Settings;
    - Windows: install VB-CABLE, select "CABLE Output" as the microphone;
    - Linux: the auto-created "Sound-Babound Mic", the `pactl` requirement;
    - selecting the device in Zoom, Teams or Discord;
  - the `[mic]` config reference;
  - the tray items;
  - `mic-check`;
  - the startup fallback (if Task 4 did not already cover it).
- [x] CLAUDE.md:
  - the new modules (passthrough, capture, engine `platform/`);
  - the three-role supervisor and the per-role policy table;
  - the play grace window (this replaces the strict "rejected while false" wording);
  - the startup grace;
  - the passthrough realtime rules and the drift controller;
  - the updated platform-code locations;
  - the bundle script;
  - the no-real-audio rule for agents.
  (README: features, the tray menu, `play` with the mic, a "Checking the virtual microphone" section for `mic-check`, the `[mic]` reference table, a "Virtual microphone" section with macOS, Windows and Linux setup, the call-app choice and a tray status table, logging of device-changed errors and build-failure throttling, and the project layout; the startup fallback was already covered by Task 4. CLAUDE.md: the realtime rules now name every callback and the passthrough, with a "Passthrough rules" list for the ring, priming and the drift controller; the play grace window replaces the "rejected while false" wording; the rebuild policy has the three roles, the per-role table, the startup grace, `StreamErrorKind`, `FailureLog`, `StreamStatus` and the `Timings` defaults; the platform section lists the app's Linux helper gate and the Linux-only `pulseaudio` feature; a new "No real audio from agents" section. The module list and the bundle script were already documented by earlier tasks.)

## Post-Completion

These checks are manual and done by a human:

- On the dev Mac with BlackHole installed and `mic.enabled = true`, run the `#[ignore]` smoke test (`cargo test -p babound-engine -- --ignored`) and `sound-babound mic-check`. Confirm that all three streams build and that the report names BlackHole, the default input and their formats.
- Launch `Sound-Babound.app` from Finder, then:
  - grant microphone access;
  - select BlackHole in a call app and confirm that participants hear the sounds and your voice, and that you do not hear yourself;
  - switch the default input and output during a call and confirm that each recovers on its own.
- On Windows with VB-CABLE, and on a Linux PipeWire desktop: confirm device detection, Linux device creation and removal, and the tray status.
- Set the default input to the virtual device and confirm the warning and the fallback.
- On Linux: switch the default input and confirm the capture follows within about a second; with the default input on "Sound-Babound Mic" and the capture on its fallback, unplug the fallback microphone and confirm the capture stops (no feedback); restart pipewire-pulse with the mic on and confirm the mic paths reconnect without piling up `babound-device-poll` threads; stop the server (`SIGSTOP`) and confirm that mic off and quit return within a few seconds with a warning.
