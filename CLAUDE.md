# Sound-Babound: notes for contributors

A Rust tray soundboard. Plans live in `docs/plans/`.

## Crate layout

- `crates/engine` (`babound-engine`): has no UI dependencies. Its modules are private. `lib.rs` re-exports only what the app needs: `Engine`, `MicSettings`, `MicStatus`, `CaptureStatus`, `MicProbe`, `probe_mic`, `Sound`, `StreamInfo`, `StreamRole`, `StreamStatus`, `DeviceEvent`, `RebuildPolicy`, the error types (with `BuildErrorKind`) and `VERSION`. Items that only tests use are `#[cfg(test)]`.
  - `error.rs`: `LoadError`, `BuildError` (including `DeviceNotFound` for an unmatched mic sink pattern and `FeedbackExcluded` for a refused capture), `StartError`, `WatcherError`.
  - `sound.rs`: symphonia decode into a cheap-clone `Sound` (`Arc<SoundData>`).
  - `mixer.rs`: the realtime mixer, the command queue and the garbage queue. The mic sink's mixer (`Mixer::with_passthrough`) owns a `PassthroughReader` and adds `input gain × passthrough` before the clamp, with the mono-sound mapping; the monitor's mixer (`Mixer::new`) has none. `Command::SetPathGains { sound, input }` sets the per-path gains (default 1.0).
  - `convert.rs`: f32 to device sample format (`OutputSample`). Every cpal PCM format is supported. I24 and U24 go through i32, because dasp overflows them at +1.0. `InputSample` is the reverse (device format to f32) for the capture.
  - `passthrough.rs`: the mic passthrough. `PassthroughShared` holds a mono `ArrayQueue<f32>` ring (0.5 s at 192 kHz) that the capture writes to. `PassthroughReader`, owned by the mic mixer, primes to 30 ms and resamples by linear interpolation. A PI controller, clamped to ±0.2 %, adjusts the ratio to hold the fill. Drift tests simulate both device clocks.
  - `sink.rs`: `StreamRole`, the `StreamFactory`, `Endpoint` and `ActiveStream` traits, `MixerEndpoint` (the endpoint of both output roles) and `CaptureEndpoint` (around `PassthroughShared`).
  - `cpal_sink.rs`: `CpalOutputFactory`, the cpal output factory of both output roles. `OutputTarget::Default` is the monitor (the default output of `platform::monitor_host()`); `OutputTarget::Named(pattern)` is the mic sink (the first output of `platform::mic_host()` whose display name or id contains the pattern, ignoring case; a blank pattern matches nothing). Both re-query on every build and identity query. `probe()` resolves the device and its default config without a stream. It also holds the helpers the capture shares: error mapping, `identity_of` and `name_matches`.
  - `capture.rs`: `CpalCaptureFactory`, the capture on the default input of `platform::mic_host()`. The pure `choose_input` refuses an input that an exclusion names (the virtual device or its monitor): it falls back to the last physical default input if it is still present, otherwise the build fails with `FeedbackExcluded` and the supervisor retries. `NoticeLog` warns once per change of that decision. A build calls `begin_capture(rate)` before the stream starts.
  - `platform/`: the cpal host per role. The monitor uses the default host, except ALSA on Linux (as in stage 1, since the `pulseaudio` feature makes PulseAudio cpal's Linux default). The mic paths use the default host, except PulseAudio on Linux (falling back to the default host).
  - `supervisor.rs`: the pure `SupervisorCore` state machine, the pure `FailureLog` build-failure throttle, `StreamStatus`, and the one generic supervisor thread (`spawn_supervisor`), one instance per role, each starting its own watcher.
  - `watcher/`: OS device-change listeners.
  - `engine.rs`: the public `Engine` API (non-blocking queue pushes and atomic reads). The monitor always runs. `set_mic(&mut self, Option<MicSettings>)` starts, stops or restarts the mic sink and capture supervisors (a gains-only change just sends `SetPathGains`); the monitor is never touched. `play` fans out to every path that accepts plays (true if any did); `stop_all` and `set_master_gain` go to every path. Factories and watchers come from the `Backends` trait: `CpalBackends` in production, `FakeBackends` in tests. `probe_mic` resolves the mic devices without building a stream.
  - `test_support.rs` (`#[cfg(test)]`): `FakeSinkFactory`, `FakeCaptureFactory`, `FakeBackends`, `FakeWatcher` and `wait_until`.
  - `testdata/`: tiny FLAC, OGG Vorbis and MP3 fixtures for the decode tests. The ffmpeg commands that made them are in the `fixture` helper's doc comment in `sound.rs`.
- `crates/app` (`sound-babound`): the binary.
  - `main.rs`: the Windows GUI subsystem, plus attaching to the parent console when any argument is given.
  - `logging.rs`: `tracing` to stderr, with an `EnvFilter` read from `RUST_LOG` (default `info`).
  - `cli.rs`: the clap CLI and headless `play`.
  - `config.rs`, `paths.rs`, `hotkeys.rs`: the TOML model, the default template, path resolution, hotkey parsing and validation. The `[mic]` table resolves into `MicConfig` (gains clamped; a missing or blank `virtual_device` takes `platform::default_virtual_device()`). `config::set_mic_enabled` rewrites only `[mic] enabled` through `toml_edit`, keeping comments, and replaces the file atomically (temp file and rename).
  - `actions.rs`: a pure `ActionMap`, the `AudioControl` trait (`play`, `set_master_gain`, `set_mic`) and `apply_config(control, map, capture)`, which sends the master gain and `mic_settings` (`None` when the mic is off; exclusions from `platform::virtual_input_exclusions`). `CaptureGate` turns `platform::microphone_access()` into the `capture` flag: only `Denied` turns it off (the mic sink still plays sounds), with one warning per change while the mic is enabled. `reload` takes the capture decision as a closure over the new `MicConfig`. `toggle_mic` flips `[mic] enabled`, saves it with `set_mic_enabled` (a failed save is logged, the change still applies) and applies the mic path. `mic_status_label` and `mic_toggle_label` are the pure tray texts. `WithVirtualMic` wraps an `AudioControl` and keeps the Linux `VirtualMic` in step with `set_mic`: it creates the device before the mic path starts (a failure is logged, and the mic sink then reports not found), drops it after the mic path stops, and on `shutdown` stops the engine before dropping the device.
  - `tray_app.rs`: the tao event loop, the tray, hotkey registration and reload. The menu has a disabled mic status line and a "Mic to call: on/off" toggle. While the mic is enabled, the loop wakes every 1 s (`ControlFlow::WaitUntil`, through the pure `mic_poll`) and updates the status text only when it changed; with the mic off it stays on `Wait`. `Hotkeys` goes through a small `HotkeyBackend` trait, so registration and reload can be tested with a fake.
  - `test_support.rs` (`#[cfg(test)]`): `TempDir`.
  - `icon.rs`: the generated tray icon.
  - `platform/`: per-OS glue. On Linux, `VirtualMic::create()` loads a `module-null-sink` (`sound_babound_sink`) and a `module-remap-source` ("Sound-Babound Mic", `sound_babound_mic`) through `pactl`, after unloading modules a crashed run left behind; the guard unloads them in reverse order on drop. The argument lists, the `pactl list short modules` parser and the create/drop sequence (`PactlDevice`, with an injected runner) are pure helpers in `platform/mod.rs`, gated `cfg(any(target_os = "linux", test))`. Elsewhere `create_virtual_mic()` returns `Ok(None)`. `microphone_access()` returns `MicrophoneAccess`: on macOS it reads `AVAudioApplication.recordPermission` (macOS 14 and later, through `objc2-avf-audio`; `Unknown` before), which never prompts; elsewhere it is `Granted`.
  - `macos/Info.plist`: the app bundle's plist template (`LSUIElement`, `NSMicrophoneUsageDescription`, `@VERSION@`). `scripts/bundle-macos.sh` builds the release binary, assembles `target/release/Sound-Babound.app`, substitutes the version, lints the plist and signs it ad hoc so that TCC binds the microphone permission to the bundle. A unit test in `platform/mod.rs` checks the plist keys.
- The embedded demo `crates/app/assets/demo.wav` is generated by `cargo run -p babound-engine --example gen_demo_wav`.

## Realtime rules

These apply to the cpal data callback and everything `Mixer::render` reaches:

- No allocation and no freeing. Never drop the last `Arc` of a `Sound` there.
- No blocking lock. Use only `try_lock`, and output silence when it fails.
- No I/O and no `tracing` macros.
- The only `push` calls allowed are on the lock-free `ArrayQueue`s.

Pending-release rule: every `Sound` that leaves a voice (finished, evicted or stopped) is pushed onto the garbage queue. `drain_garbage()` frees those sounds on the supervisor thread. If the garbage queue is full, the `Sound` goes into a preallocated pending-release slot (`2 * MAX_VOICES` slots). Those slots are retried at the start of every render. Commands are popped only while at least `MAX_VOICES` slots are free, so every release always has somewhere to go. A finished voice with no free slot stays parked, silent and uncounted, until a slot frees up.

## Mixer handoff invariant

The mixer lives in `Arc<Mutex<Mixer>>`. To rebuild, the supervisor drops the old sink and then calls `StreamFactory::build`, so `build` only ever runs while no stream exists. `build` may lock the mixer freely, and it must call `configure(rate, channels)` before it starts the stream. Once the stream runs, only its callback touches the mixer, through `try_lock`, so that lock never contends. `configure` keeps the voices and their source positions, which is how playback continues across a rebuild.

Only a running stream drains the command queue. `MixerShared::output_active` (`Engine::has_output`) reports whether a stream runs; the supervisor sets it only after `build` returns. `Engine::play` rejects a play (returns false) while it is false, so presses made with no device, or during a build started with no stream, never start late when a stream comes up. A rebuild from a live stream keeps the command queue: plays accepted while the old stream ran, including one whose push races the teardown (the check and the push are not atomic), are applied by the new stream, like the voices it continues. While no stream exists (failed build, retry backoff), each supervisor loop, and every build started with no stream, locks the mixer, which cannot contend then, and calls `Mixer::apply_offline`. It drops any play that passed the check just before the stream went away, and it still applies `StopAll` and `SetMasterGain`. The headless `play` waits for `has_output` before queuing its play, and once the mixer reports no voices it keeps the stream up for the measured output latency (`Engine::output_latency`: the cpal callback timestamps' `playback - callback` plus the duration of the callback's buffer, so it reaches the last written frame; capped at 5 s) plus a 200 ms margin, so the device buffer is heard.

## Rebuild policy

A rebuild is an audible gap, so not every event triggers one. Events are debounced on the trailing edge, 200 ms after the last event.

- Always rebuild: `DefaultOutputChanged`, a forwarded stream error, and a stalled callback heartbeat (no progress for 2 s). The cpal error callback filters stream errors first (`cpal_sink::needs_rebuild`). `Xrun` and `RealtimeDenied` are dropped, because the stream keeps running and a rebuild would only add a gap.
- Identity-checked (`IfIdentityChanged`): `DeviceListChanged`, `DeviceStateChanged` and `DeviceRemoved`. The supervisor re-queries the live default output identity (`StreamFactory::current_identity`) and rebuilds only if it differs from the identity the stream was built on (`None` vs `Some` counts as different). Otherwise it logs the event at debug and drops it.
- Ignored: `DefaultInputChanged`.
- With no active stream (initial start, or the 1 s retry backoff after a failed build), every non-ignored event rebuilds.
- Never cache the output device. Every build and every identity check re-queries the default device.

The classification is `DeviceEvent::rebuild_policy(role)`; the rules above are the monitor's (the mic sink ignores both default changes, the capture rebuilds on `DefaultInputChanged` and ignores `DefaultOutputChanged`; all roles identity-check list, state and removed events). The timing logic is the pure `SupervisorCore`, tested with a fake clock.

## Platform code

Platform code lives only in:

- `crates/engine/src/watcher/{macos,windows,linux}.rs`
- `crates/engine/src/platform/{macos,windows,linux}.rs`
- `crates/app/src/platform/{macos,windows,linux}.rs`

Each set is selected by `#[cfg(target_os = ...)]` in its `mod.rs`. Shared code contains no `cfg(target_os)`. The only `target_os` cfgs outside the per-OS files are in those `mod.rs` files: the watcher selection, `NoopWatcher` (built only outside macOS and Windows), the engine host selection (with a default-host fallback for other OSes), and a dead-code allow on `EventSender::send`, which only the macOS and Windows watchers call.

The macOS watcher uses `objc2-core-audio` rather than `coreaudio-sys`, which would need libclang. The Linux watcher is a no-op, because ALSA `default` follows PipeWire/PulseAudio.

## Conventions

- Edition 2024, workspace resolver 3.
- No `unwrap`/`expect` outside tests.
- Typed `thiserror` errors in the engine (`LoadError`, `BuildError`, `StartError`, `WatcherError`); `anyhow` in the app.
- Tests use fakes (`FakeSinkFactory`, `FakeWatcher`, a recording `AudioControl`, a fake `HotkeyBackend`) and need no audio device. The one real-device smoke test is `#[ignore]`.

## Verification

Run all of these after every change. They must all be clean:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets          # zero warnings
cargo test --workspace
cargo check --workspace --target x86_64-pc-windows-msvc
```

A Linux cross-check from macOS is blocked by `alsa-sys` pkg-config. It only type-checks if you give it a stub `PKG_CONFIG` and set `PKG_CONFIG_ALLOW_CROSS=1`. The stub is a shell script that prints `99.0` for `--modversion` and otherwise exits 0. With it, run `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu`. It catches Linux-only dead code, but nothing is linked.
