# Sound-Babound stage 1: hotkey soundboard with device-change-proof playback

## Overview

This builds a cross-platform tray soundboard in Rust. A global hotkey plays a preloaded sound through the system default output device. Playback must survive default-device switches and unplug/replug: a supervisor thread watches device events, debounces them, decides whether the event actually affects the active output, rebuilds the output stream against the current default device when it does, and keeps playing voices going from where they were.

The repository is greenfield. This plan creates a Cargo workspace with two crates:

- `babound-engine`: decoding, realtime mixer, cpal output sink, device supervisor, platform watchers. It has no UI dependencies.
- `sound-babound`: the binary with the tray, global hotkeys, TOML config and a headless `play` CLI.

Stage 2 (virtual mic) is out of scope. The engine is shaped so that a second sink is just one more supervisor instance with a different `SinkFactory`, with the engine fanning commands out to both. Nothing for stage 2 is built now.

## Context

- Existing files: `.gitignore` (Cargo), `LICENSE` (MIT), `README.md` (title only). There is no code yet.
- New files (planned layout):
  - `Cargo.toml` (workspace, release profile)
  - `crates/engine/` (`babound-engine`): `src/lib.rs`, `src/error.rs`, `src/sound.rs`, `src/mixer.rs`, `src/convert.rs`, `src/supervisor.rs`, `src/sink.rs`, `src/cpal_sink.rs`, `src/engine.rs`, `src/watcher/{mod.rs,macos.rs,windows.rs,linux.rs}`, `examples/gen_demo_wav.rs`
  - `crates/app/` (`sound-babound`): `src/main.rs`, `src/cli.rs`, `src/logging.rs`, `src/config.rs`, `src/paths.rs`, `src/hotkeys.rs`, `src/actions.rs`, `src/tray_app.rs`, `src/icon.rs`, `src/platform/{mod.rs,macos.rs,windows.rs,linux.rs}`, `assets/demo.wav`
  - `CLAUDE.md`, `README.md`
- Toolchain: Rust 1.98 stable with targets `aarch64-apple-darwin` and `x86_64-pc-windows-msvc` installed. No Linux or Windows arm64 target is installed.
- Dependencies (latest stable at time of writing):
  - Engine: cpal 0.18, symphonia 0.6 (features wav, pcm, flac, ogg, vorbis, mp3), crossbeam-queue 0.3 (lock-free `ArrayQueue`), thiserror, tracing.
  - Engine on macOS: objc2-core-audio 0.3 and objc2-core-foundation 0.3.
  - Engine on Windows: windows 0.62 with Win32 Media Audio, COM and PropertiesSystem features, plus windows-core with the `implement` macro.
  - App: tao 0.37, tray-icon 0.26, global-hotkey 0.9, serde, toml, dirs, clap (derive), anyhow, tracing-subscriber (env-filter), opener.
- Documented deviation from the conventions: the macOS watcher uses `objc2-core-audio` instead of `coreaudio-sys`.
  - `coreaudio-sys` generates its bindings with bindgen in its build script, which needs libclang at build time.
  - cpal 0.18 already depends on `objc2-core-audio` through coreaudio-rs 0.14. Using it adds no new bindings and needs no libclang.

## Development Approach

- **Testing approach**: Regular (code first, then tests in the same task).
- Complete each task fully before moving to the next.
- Use edition 2024 (latest stable edition), workspace `resolver = "3"`.
- Release profile: `lto = true`, `codegen-units = 1`, `strip = true`, default `opt-level` (3). No size-oriented opt-level: the mixer render loop is the hot path and must keep vectorization; binary size comes from LTO and strip.
- Realtime rules. Inside the cpal data callback and `Mixer::render` there must be:
  - no allocation and no freeing (never dropping the last `Arc`, see the pending-release rule in Task 3)
  - no blocking lock (only `try_lock`)
  - no I/O and no `tracing` macros
- The mixer handoff works as follows:
  - The mixer lives in `Arc<Mutex<Mixer>>`.
  - The callback uses `try_lock` and outputs silence if the lock is unavailable.
  - The supervisor only locks the mixer while no stream exists, either before building or after dropping the stream. So the callback never contends.
- Rebuild policy (a rebuild is an audible gap, so not every event triggers one):
  - After the debounce window, `DefaultOutputChanged`, a `StreamError` and a stalled heartbeat always rebuild.
  - `DeviceListChanged`, `DeviceStateChanged` and `DeviceRemoved` rebuild only when the identity of the current default output device, re-queried at that moment, differs from the identity the active stream was built on. Otherwise the event is logged at debug and dropped.
  - This compares against the live default; the stream never binds to a cached device, so "never cache the device identity" still holds.
  - Input-device events never rebuild.
- Platform code lives only in `watcher/{macos,windows,linux}.rs` (engine) and `platform/{macos,windows,linux}.rs` (app), selected by `#[cfg(target_os = ...)]` in the module's `mod.rs`. Shared code contains no `cfg(target_os)`.
- Error handling:
  - No `unwrap`/`expect` on runtime paths. They are allowed in tests only.
  - Typed `thiserror` errors inside the engine (`LoadError`, `BuildError`); `anyhow` in the app.
- After every task, run `cargo fmt --all`, `cargo clippy --workspace --all-targets` and `cargo test --workspace`, all clean.
- From Task 1 on, also run `cargo check --workspace --target x86_64-pc-windows-msvc`.
- **CRITICAL: every task MUST include new/updated tests**
- **CRITICAL: all tests must pass before starting next task**

## Implementation Steps

### Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml`, `crates/engine/Cargo.toml`, `crates/engine/src/lib.rs`, `crates/app/Cargo.toml`, `crates/app/src/main.rs`, `crates/app/src/cli.rs`, `crates/app/src/logging.rs`

- [x] Workspace `Cargo.toml`:
  - members `crates/engine` and `crates/app`
  - shared `[workspace.package]` (edition 2024, MIT license, rust-version 1.98)
  - `[workspace.dependencies]` with the versions listed in Context
  - the release profile from Development Approach (`lto = true`, `codegen-units = 1`, `strip = true`, no `opt-level` override)
- [x] `babound-engine` library crate with an empty public module skeleton. It has no UI dependencies.
- [x] `sound-babound` binary crate depending on `babound-engine`.
- [x] `cli.rs`: clap derive with no subcommand (meaning tray mode) and `play [FILE]` (no FILE means the embedded demo). Both branches are stubs for now.
- [x] `logging.rs`: `tracing-subscriber` with `EnvFilter`, read from `RUST_LOG` and defaulting to `info`.
- [x] Add `#![cfg_attr(windows, windows_subsystem = "windows")]` in `main.rs`.
- [x] Write tests: CLI parsing of no args, `play`, and `play path.wav` (clap `try_parse_from`).
- [x] Run fmt, clippy, test and the Windows-target check. All must pass before Task 2.

### Task 2: Sound decoding and embedded demo asset

**Files:**
- Create: `crates/engine/src/error.rs`, `crates/engine/src/sound.rs`, `crates/engine/examples/gen_demo_wav.rs`, `crates/app/assets/demo.wav`

- [x] `error.rs`:
  - `LoadError`: `Io`, `Unsupported`, `NoAudioTrack`, `Decode`, `Empty`; each carries the source label.
  - `BuildError`: `NoDevice`, `UnsupportedFormat`, `Backend(String)`.
- [x] `sound.rs`: a `Sound` handle that is a cheap-clone `Arc<SoundData>`. `SoundData` holds:
  - interleaved `Vec<f32>` samples
  - `sample_rate`
  - `channels`
  - `frames`
  - display label
- [x] `Sound::load_file(path)` and `Sound::load_bytes(bytes, label)`:
  - Decode the whole file with symphonia (WAV/FLAC/OGG-Vorbis/MP3) into interleaved f32 at native rate and channel count.
  - Tolerate recoverable decode errors mid-stream, but return `Err` when nothing could be decoded.
  - Never panic.
- [x] `examples/gen_demo_wav.rs`:
  - A deterministic synthesizer for a ~1.5 s two-tone chime with exponential decay: 44.1 kHz, mono, 16-bit PCM, with a hand-written WAV header and no new dependency.
  - Run it once to produce `crates/app/assets/demo.wav` and commit the file.
  - Mono at 44.1 kHz deliberately exercises resampling and mono-to-stereo mapping.
- [x] Write tests (tests write small WAV byte buffers with an in-test helper):
  - mono 44.1 kHz and stereo 48 kHz decode to the correct frames, rate and channels
  - sample values round-trip within tolerance
  - garbage bytes give `LoadError`, not a panic
  - an empty data chunk gives `LoadError::Empty`
  - a missing file gives `LoadError::Io`
  - the committed `demo.wav` (via `include_bytes!` in the test) decodes
- [x] Run the test suite. It must pass before Task 3.

### Task 3: Realtime mixer

**Files:**
- Create: `crates/engine/src/mixer.rs`

- [x] Types:
  - `Command` enum: `Play { sound: Sound, gain: f32 }`, `StopAll`, `SetMasterGain(f32)`.
  - `MixerShared`: holds the lock-free `ArrayQueue<Command>` (capacity 64), a garbage `ArrayQueue<Sound>` (capacity 128), atomics for `active_voices`, `commands_applied` and `callback_heartbeat`, and `drain_garbage()` (called by the supervisor thread, drops the queued `Sound`s off the realtime thread).
- [x] `Mixer` structure:
  - fixed-capacity voice storage (`MAX_VOICES = 32`, preallocated)
  - voice fields: sound, gain, source position in source frames (`f64`), start sequence number
  - a fixed-size pending-release slot array `[Option<Sound>; PENDING_RELEASE]` with `PENDING_RELEASE = 2 * MAX_VOICES`, preallocated, never resized
  - master gain
  - output rate and channel count
  - a scratch buffer preallocated in `configure()`
- [x] `Mixer::configure(rate, channels)` runs outside the callback and may allocate. It keeps the voices and their source positions, so playback continues after a rebuild.
- [x] Release rule (an `Arc` is never dropped on the realtime thread):
  - Every `Sound` leaving a voice (finished, evicted, stopped) is pushed into the garbage queue.
  - If the garbage queue is full, the `Sound` is moved into a free pending-release slot instead (no allocation).
  - At the start of every `render`, pending-release slots are retried in order and pushed into the garbage queue while it has room.
  - Commands are popped only while at least `MAX_VOICES` pending-release slots are free (the worst case of one `StopAll` or eviction burst). Otherwise the remaining commands stay queued for the next render. This keeps a free slot available for every release, so no `Sound` is ever dropped in `render`.
  - Voices that finish while no slot is free stay in their voice slot, marked finished and silent, are not counted in `active_voices`, and are released on a later render.
- [x] `Mixer::render(&mut self, out: &mut [f32])`:
  - Retry pending releases, then drain commands under the release rule. `Play` beyond 32 voices evicts the voice with the lowest start sequence.
  - Mix with linear interpolation at step `src_rate / dev_rate`.
  - Channel mapping:
    - mono source: device channels 0 and 1 (only 0 on a mono device)
    - stereo source: L to 0 and R to 1, or the average on a mono device
    - more than 2 source channels: use the first two
    - extra device channels: silent
  - Apply per-voice gain × master gain, then clamp to [-1, 1].
  - Release finished, evicted and stopped voices' `Sound` via the release rule.
  - Update `active_voices`, `commands_applied` and `callback_heartbeat`.
- [x] Write tests:
  - 33rd voice evicts the oldest
  - overlapping plays of the same sound sum, with no restart
  - `StopAll` silences on the next render and moves sounds to the garbage queue
  - a 44.1 kHz source on a 48 kHz output consumes frames at a ratio of 0.91875, and the 48k→44.1k inverse
  - linear interpolation values at fractional positions
  - mono → stereo duplicates; stereo → mono averages; stereo → 6-channel leaves channels 2..5 silent
  - per-sound gain and master gain multiply
  - `configure()` mid-playback preserves position
  - a full command queue makes push return the command back (no panic)
  - full garbage queue: pre-fill the garbage queue to capacity, play a short sound and render past its end; the test-held clone's `Arc::strong_count` shows the mixer still holds a reference (no drop in render) and the sound sits in a pending-release slot; after `drain_garbage()` and one more render plus drain, the strong count drops to 1 (released off the realtime thread)
  - with pending-release slots nearly full, commands stay queued and are applied on a later render once slots free
- [x] Run the test suite. It must pass before Task 4.

### Task 4: Sink and watcher traits, device supervisor

**Files:**
- Create: `crates/engine/src/sink.rs`, `crates/engine/src/supervisor.rs`, `crates/engine/src/watcher/mod.rs`

- [x] `watcher/mod.rs`:
  - `DeviceEvent` enum: `DefaultOutputChanged`, `DefaultInputChanged`, `DeviceListChanged`, `DeviceRemoved`, `DeviceStateChanged`.
  - `RebuildPolicy` enum and the pure classification helper `DeviceEvent::rebuild_policy()`:
    - `DefaultOutputChanged` → `Always`
    - `DeviceListChanged`, `DeviceStateChanged`, `DeviceRemoved` → `IfDefaultOutputChanged`
    - `DefaultInputChanged` → `Ignore`
  - `DeviceWatcher` trait: `start(sender) -> Result<Box<dyn WatcherGuard>, ...>`. The guard unregisters on drop.
  - `platform_watcher()` selected by cfg. Platform modules come in Task 6. For now Linux and the default use a no-op watcher.
- [x] `sink.rs`:
  - `DeviceIdentity`: an opaque comparable value (cpal's device id when the backend provides a stable one, otherwise the device name).
  - `StreamInfo` with device identity, device name, sample rate, channels and sample format.
  - `SinkFactory` trait, `Send + 'static`:
    - `build(&mut self, mixer: Arc<Mutex<Mixer>>, on_error: Sender<SupervisorMsg>) -> Result<(Box<dyn ActiveSink>, StreamInfo), BuildError>`
    - `current_default_identity(&mut self) -> Option<DeviceIdentity>`: re-queries the live default output every call; `None` means no output device.
  - `ActiveSink` is non-`Send` and owned by the supervisor thread. Dropping it stops the stream.
- [x] `supervisor.rs`, part 1: a pure state machine `SupervisorCore` driven by explicit `Instant`s.
  - Methods: `on_event(policy, now)`, `on_stream_error(now)`, `on_build_result(Option<DeviceIdentity>, now)` (`Some` on success, recording the built-on identity; `None` on failure), `poll(now) -> Option<Action>`, `resolve_check(current: Option<DeviceIdentity>) -> CheckOutcome`, `next_deadline()`.
  - `Action` is `Rebuild` or `CheckDefault`; `CheckOutcome` is `Rebuild` or `Drop`.
  - Debounce is trailing-edge, 200 ms after the last event. A window containing any `Always` event or a stream error yields `Rebuild`; a window with only `IfDefaultOutputChanged` events yields `CheckDefault`; `Ignore` events do not open a window.
  - `resolve_check` returns `Rebuild` when the current default identity differs from the built-on identity (including `None` vs `Some`), otherwise `Drop`.
  - When no stream is active (initial start or retry backoff), every non-ignored event leads to `Rebuild`.
  - Retry backoff is 1 s after a failed build. New events during the backoff still debounce.
  - The stall watchdog yields `Rebuild` if `callback_heartbeat` has not advanced for 2 s while a stream is supposed to be running.
- [x] `supervisor.rs`, part 2: `spawn_supervisor(factory, watcher, shared)` starts a dedicated thread.
  - The thread loop is a `recv_timeout` on one channel carrying watcher events, stream errors and `Shutdown`.
  - The thread builds immediately on start.
  - On `CheckDefault` it calls `factory.current_default_identity()` and passes the result to `resolve_check`; on `Drop` it logs the event at debug and does nothing else.
  - Rebuild sequence: drop the sink, lock the mixer and call `configure(info)`, then build.
  - It calls `drain_garbage()` each loop iteration (at most every 100 ms).
  - It logs rebuild outcomes at info: device name, rate, channels, format.
  - It logs input-device events at debug and ignores them.
- [x] Write tests for the classification helper: every `DeviceEvent` variant maps to the policy listed above.
- [x] Write tests with a fake clock against `SupervisorCore`:
  - bursts of 5 events within 200 ms cause exactly one rebuild
  - failure leads to retry at 1 s; repeated failures keep retrying; success stops the retries
  - a stalled heartbeat triggers a rebuild
  - a stream error always triggers a rebuild
  - input-only events do not trigger a rebuild
  - plugging an unrelated device (`DeviceListChanged`) while the default output identity is unchanged: `poll` yields `CheckDefault`, `resolve_check` yields `Drop`, no rebuild
  - `DeviceListChanged` where the default output identity changed yields a rebuild; the same for `DeviceRemoved` with the current default now `None`
  - a burst mixing `DeviceListChanged` and `DefaultOutputChanged` rebuilds without a check
- [x] Write thread-level tests with `FakeWatcher` and `FakeSinkFactory` (fails N times, then succeeds; settable current default identity; records builds):
  - a `DefaultOutputChanged` event leads to a rebuild with the new fake rate
  - a `DeviceListChanged` event with an unchanged fake default identity produces no new build
  - a playing voice keeps its source position across the rebuild
  - shutdown joins promptly
- [x] Run the test suite. It must pass before Task 5.

### Task 5: cpal output sink with sample-format conversion

**Files:**
- Create: `crates/engine/src/cpal_sink.rs`, `crates/engine/src/convert.rs`

- [ ] `convert.rs`: pure functions that write an f32 mix into `&mut [T]` for `f32`, `i16`, `u16`, plus `i32` and `u8` via cpal's `FromSample`. They do not allocate.
- [ ] `CpalSinkFactory::build`:
  - Query `host.default_output_device()` on every call and never cache it. No device means `BuildError::NoDevice`.
  - Use `default_output_config()` for format, rate and channels.
  - Build a typed stream per `SampleFormat`. Unknown formats give `BuildError::UnsupportedFormat`.
  - Data callback: `try_lock` the mixer; render in chunks of the preallocated scratch size, then convert; on lock failure, fill with silence.
  - Error callback: send `SupervisorMsg::StreamError` (no logging inside it beyond the send).
  - Call `play()` and return `StreamInfo` with the device identity and name.
- [ ] `CpalSinkFactory::current_default_identity`: re-query `host.default_output_device()` and derive its `DeviceIdentity`; nothing is stored between calls.
- [ ] Write tests:
  - conversion boundaries (−1, 0, 1, clipping) for i16 and u16
  - a chunked render over a buffer larger than the scratch size matches a single render
  - a `#[ignore]` smoke test that builds a real stream on the dev machine and checks `current_default_identity()` equals the built stream's identity
- [ ] Run the test suite and the Windows-target check. They must pass before Task 6.

### Task 6: Platform device watchers

**Files:**
- Create: `crates/engine/src/watcher/macos.rs`, `crates/engine/src/watcher/windows.rs`, `crates/engine/src/watcher/linux.rs`
- Modify: `crates/engine/src/watcher/mod.rs`, `crates/engine/Cargo.toml`

- [ ] macOS:
  - Call `AudioObjectAddPropertyListener` on `kAudioObjectSystemObject` for `DefaultOutputDevice`, `DefaultInputDevice` and `Devices`.
  - The client data is a boxed `Sender`; the `extern "C"` listener maps the selector to a `DeviceEvent` (`Devices` → `DeviceListChanged`, which goes through the identity check from Task 4).
  - The guard removes all three listeners and then frees the box.
- [ ] Windows:
  - A dedicated thread does `CoInitializeEx(COINIT_MULTITHREADED)`, creates `IMMDeviceEnumerator` and registers an `IMMNotificationClient` implemented with `#[implement]`.
  - Event mapping:
    - `OnDefaultDeviceChanged`: eRender to `DefaultOutputChanged`, eCapture to `DefaultInputChanged`; react only to the eConsole role to avoid duplicates.
    - `OnDeviceRemoved` maps to `DeviceRemoved`.
    - `OnDeviceStateChanged` maps to `DeviceStateChanged`.
    - `OnDeviceAdded` maps to `DeviceListChanged`.
    - `OnPropertyValueChanged` is ignored.
  - Removal, state and add events are not filtered here; the supervisor's identity check (Task 4) decides whether they rebuild.
  - The guard signals the thread to unregister, then `CoUninitialize` and join.
- [ ] Linux: a no-op watcher with a doc comment. ALSA `default` routes through PipeWire/PulseAudio, which follows default-sink changes; stream errors still reach the supervisor.
- [ ] `mod.rs`: `platform_watcher()` dispatches by `cfg(target_os)`; the no-op is also the fallback for other OSes.
- [ ] Write tests:
  - macOS: start the watcher and drop the guard without error or leak (`cfg(target_os = "macos")`)
  - shared: extend the Task 4 classification tests so every event variant the platform watchers emit is covered by a `rebuild_policy()` assertion
- [ ] Run the test suite and `cargo check --target x86_64-pc-windows-msvc`. They must pass before Task 7.

### Task 7: Public engine API

**Files:**
- Create: `crates/engine/src/engine.rs`
- Modify: `crates/engine/src/lib.rs`

- [ ] `Engine::start() -> Result<Engine, ...>` uses the cpal factory and the platform watcher. A `pub(crate) start_with(factory, watcher)` exists for tests.
- [ ] Methods: `play(&Sound, gain) -> bool` (false if the queue is full, logged by the caller side), `stop_all()`, `set_master_gain(f32)`, `active_voices()`, `commands_applied()`, `shutdown(self)`. `Drop` also shuts down.
- [ ] All methods are non-blocking queue pushes or atomic reads, and `Engine` is `Send + Sync` so it can be shared via `Arc`. `shutdown` signals the thread and joins it.
- [ ] Re-export the public types from `lib.rs`: `Engine`, `Sound`, `LoadError`, `BuildError`, `DeviceEvent`, `StreamInfo`.
- [ ] Write tests with the fake factory and watcher, rendering through the fake sink's handle to the mixer:
  - `play` then render produces non-zero output
  - `stop_all` silences
  - `set_master_gain(0)` silences
  - the engine is `Send + Sync` (static assertion)
  - play/stop calls from multiple threads do not panic
- [ ] Run the test suite. It must pass before Task 8.

### Task 8: Configuration and hotkey parsing

**Files:**
- Create: `crates/app/src/config.rs`, `crates/app/src/paths.rs`, `crates/app/src/hotkeys.rs`

- [ ] `paths.rs`: `config_dir()` is `dirs::config_dir()/sound-babound`; `config_file()` is `config.toml` inside it; `ensure_default_config()` writes the commented template if the file is missing.
- [ ] `config.rs`, raw serde model:
  - `master_gain`, default 1.0
  - `stop_all_hotkey`
  - `[[sound]]` entries with `name`, `path`, `hotkey` and optional `gain` (default 1.0)
- [ ] `config.rs`, template and path resolution:
  - The `DEFAULT_CONFIG` template binds `path = "builtin:demo"` to `CmdOrCtrl+Shift+1`, sets stop-all to `CmdOrCtrl+Shift+0`, and explains every field and the hotkey syntax in comments.
  - Path resolution: `builtin:demo` maps to the embedded asset; absolute paths are used as-is; relative paths are joined to the config dir.
- [ ] `hotkeys.rs`:
  - Parse via `global_hotkey::hotkey::HotKey::from_str`.
  - If the parser rejects a documented spelling (`Ctrl+Shift+1`, `Alt+F5`, `Cmd+Shift+A`), add a small alias-normalization step before parsing.
  - Validation returns a `ResolvedConfig` plus a list of warnings. Unparsable hotkeys and duplicate hotkeys (the second one onward) are skipped with a warning, the sound stays playable from the tray, and gains (per-sound and master) are clamped to [0, 4].
- [ ] Write tests:
  - the default template parses and references `builtin:demo`
  - each example hotkey parses
  - garbage hotkeys produce a warning and no crash
  - duplicates are detected
  - path resolution in all three forms
  - missing optional fields take their defaults
  - malformed TOML gives an error with a readable message
  - `ensure_default_config` in a temp dir creates the file once and does not overwrite it
- [ ] Run the test suite. It must pass before Task 9.

### Task 9: Headless `play` CLI

**Files:**
- Modify: `crates/app/src/main.rs`, `crates/app/src/cli.rs`
- Create: `crates/app/src/platform/mod.rs`, `crates/app/src/platform/windows.rs` (plus macOS and Linux stubs)

- [ ] `run_play(file: Option<PathBuf>) -> anyhow::Result<()>`:
  - Decode first, either the file or the embedded `include_bytes!("../assets/demo.wav")`. A decode failure returns a contextual error before any audio starts.
  - Start the `Engine` and call `play`.
  - Wait until `commands_applied >= 1 && active_voices == 0`, polling every 50 ms and handling Ctrl+C via the default process behaviour.
  - Shut down, then return.
- [ ] `main` returns `anyhow::Result`, so errors print a readable message and exit with code 1.
- [ ] Windows: `platform::attach_parent_console()` calls `AttachConsole(ATTACH_PARENT_PROCESS)` for CLI mode, so output is visible despite the GUI subsystem. It is a no-op elsewhere.
- [ ] Write tests: `run_play` on a non-audio temp file returns `Err` whose message names the file, without touching audio; the embedded demo decodes through the same path the CLI uses.
- [ ] Run the test suite and the Windows-target check. They must pass before Task 10.

### Task 10: Tray application and global hotkeys

**Files:**
- Create: `crates/app/src/tray_app.rs`, `crates/app/src/actions.rs`, `crates/app/src/icon.rs`
- Modify: `crates/app/src/main.rs`, `crates/app/src/platform/{mod.rs,macos.rs,linux.rs}`

- [ ] `actions.rs`:
  - An `Action` enum with `Play(index)`, `StopAll`, `Reload`, `OpenConfigFolder` and `Quit`.
  - An `ActionMap` built from `ResolvedConfig` that maps menu item IDs and hotkey IDs to actions and carries the resolved `master_gain`. This logic is pure and testable.
  - A two-method `AudioControl` trait (`play`, `set_master_gain`) implemented for `Engine`, and `apply_config(&impl AudioControl, &ActionMap)` that calls `set_master_gain(map.master_gain())`. Startup and every successful reload go through `apply_config`; a failed reload does not call it.
- [ ] `icon.rs`: a programmatically generated 32×32 RGBA tray icon (no image asset or decoder dependency), set as a template icon on macOS.
- [ ] `tray_app.rs`, startup:
  - A tao `EventLoop` with a user-event type.
  - On macOS, `ActivationPolicy::Accessory` (no Dock icon) via `platform::macos`.
  - Create the tray and menu inside the `StartCause::Init` handler.
  - The menu lists the sounds, then "Stop all", "Reload config", "Open config folder" and "Quit".
  - `MenuEvent` and `GlobalHotKeyEvent` handlers forward events through the `EventLoopProxy`.
- [ ] `tray_app.rs`, hotkeys, reload and actions:
  - Hotkeys: `GlobalHotKeyManager` (Carbon on macOS, so no Accessibility permission). Each registration failure is logged and skipped.
  - On Linux, `platform::linux::is_wayland_session()` checks `XDG_SESSION_TYPE` and `WAYLAND_DISPLAY`. If it is Wayland, skip the manager and log a clear warning; the tray still works.
  - Reload:
    - Re-read the config. On a parse error, keep the old config and log it.
    - Decode the sounds synchronously (per-file errors logged, the entry dropped).
    - Unregister the old hotkeys, register the new ones, rebuild the menu with `set_menu`, swap the `ActionMap`.
    - Call `apply_config` so the new `master_gain` reaches the engine via `set_master_gain`.
    - The engine is otherwise untouched; voices that are playing continue, and their `Arc`s are freed via the garbage queue.
  - "Open config folder" uses the `opener` crate.
  - "Quit" shuts down the engine, then exits the loop.
- [ ] Startup sequence: `ensure_default_config`, load (falling back to the embedded demo only if the file is unreadable), start the engine, call `apply_config` (sets `master_gain`), then run the loop.
- [ ] Write tests:
  - `ActionMap` construction (indices, stop-all mapping, entries with skipped hotkeys still appear in the menu, `master_gain` carried from the config including clamping)
  - `apply_config` with a recording fake `AudioControl`: startup applies the config's `master_gain`; a successful reload applies the new value; a failed reload leaves the fake untouched
  - Wayland detection over injected env values
  - icon buffer size and alpha
- [ ] Run the test suite and the Windows-target check. They must pass before Task 11.

### Task 11: Verify acceptance criteria

- [ ] Run `cargo build --workspace` and `cargo test --workspace` (macOS arm64).
- [ ] Run `cargo clippy --workspace --all-targets` (zero warnings) and `cargo fmt --all -- --check`.
- [ ] Run `cargo check --workspace --target x86_64-pc-windows-msvc`.
- [ ] Try `cargo check -p babound-engine --target x86_64-unknown-linux-gnu`, adding the target if needed. If it is blocked by pkg-config/ALSA cross-sysroot, record that and instead grep that every `cfg(target_os = "linux")` item has a matching module and that the shared code contains no `target_os` cfg.
- [ ] Check there are no `unwrap(`/`expect(` outside `#[cfg(test)]` code (grep).
- [ ] Check that the callback path (`mixer.rs` render path, `cpal_sink.rs` data closure) contains no `tracing`, `Vec::new`/`push`/`collect` or `lock()` (grep).
- [ ] Verify test coverage meets 80%+ on engine logic modules (mixer, supervisor, sound, convert) and app config/hotkeys/actions. Use `cargo llvm-cov` if installed; otherwise review coverage by test inventory.

### Task 12: Update documentation

- [ ] `README.md` covers:
  - what it does
  - building (`cargo build --release`)
  - running (tray mode, `sound-babound play [FILE]`, `RUST_LOG`)
  - config location per OS and a config example
  - hotkey syntax (modifiers Ctrl/Alt/Shift/Cmd/Super/CmdOrCtrl, key names)
  - the Wayland limitation
  - Linux build dependencies (`libasound2-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev` or `libappindicator3-dev`, `libxdo-dev` if required by tray-icon)
- [ ] Create a short `CLAUDE.md` covering:
  - crate layout
  - realtime callback rules, including the pending-release rule
  - the mixer handoff invariant
  - the rebuild policy (always vs identity-checked events)
  - where platform code lives
  - the verification commands (test, clippy, Windows-target check)

## Post-Completion

These require manual verification:

- AC2: launch with `cargo run -p sound-babound`. Check the tray icon appears with no Dock icon or window, that the config is created, and that `CmdOrCtrl+Shift+1` plays the chime while another app has focus.
- AC3: play a long file with `sound-babound play long.mp3`. Switch the default output in System Settings, then plug and unplug a USB headset. Check that playback moves within about 0.5 s, and that unplugging and replugging the active device recovers playback. Also plug in an unrelated device that does not become the default and check there is no audible gap.
- AC4: rapid hotkey presses overlay; "Stop all" silences at once.
- AC5: listen to 44.1 kHz and 48 kHz files on the actual device for correct pitch; check mono plays on both channels.
- AC6: edit the config (including `master_gain`), choose "Reload config", and check that new sounds, hotkeys and gain apply.
- Windows 11 x64 and arm64: real run, after `rustup target add aarch64-pc-windows-msvc` for the arm64 check.
- Linux x86_64 and aarch64 (X11 and Wayland sessions): native build and run with the README dependencies installed.
