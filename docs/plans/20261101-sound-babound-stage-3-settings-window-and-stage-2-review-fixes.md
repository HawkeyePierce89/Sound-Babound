# Sound-Babound stage 3: settings window and stage 2 review fixes

## Overview

Two parts on one branch.

Part A adds a small settings window. It runs as a separate process, `sound-babound settings`, and does two things:

1. It shows whether the virtual device is there and which input the capture would use. This is the same information `mic-check` prints, refreshed every second.
2. It lists the configured sounds so the user can add one (file picker), remove one, rename one and assign a hotkey to each.

Save rewrites the config file atomically, through `toml_edit`, with comments kept. The tray gets a "Settings…" item. It launches the window, one instance at a time, pauses the global hotkeys while the window is open, and reloads the config when the window closes after changing the file. The window uses egui (eframe, glow) and rfd. Form state, validation, key mapping and file writing live in modules that know nothing about egui, so a native window could replace the view later.

Part B fixes the five findings from the stage 2 acceptance review:

- B.1: the passthrough carries audio on long device buffers. The reader's target adapts to the block sizes it sees, and macOS asks for 10 ms periods.
- B.2: the PulseAudio probe's connect is bounded, and a failed probe holds further probes off.
- B.3: `pactl` work moves off the tray's event loop onto a device worker thread.
- B.4: the tray's "microphone unavailable" status says why.
- B.5: the macOS default pattern prefers BlackHole 2ch.

## Context

- Engine files involved:
  - `crates/engine/src/passthrough.rs`, `mixer.rs`, `cpal_sink.rs` (B.1, B.5)
  - `platform/macos.rs` (B.1), `platform/linux.rs`, `platform/shared_host.rs` (B.2)
  - `engine.rs`, `error.rs`, `lib.rs`, `test_support.rs` (B.4, B.5)
- App files involved:
  - `crates/app/src/actions.rs`, `tray_app.rs`, `cli.rs`, `config.rs`, `hotkeys.rs`, `main.rs`, `platform/{mod,macos,windows,linux}.rs`
  - plus `README.md` and `CLAUDE.md`
- New files:
  - `crates/app/src/virtual_mic.rs`: `WithVirtualMic`, `VirtualDevice` and the device worker, moved out of `actions.rs` (2100 lines) and made asynchronous.
  - `crates/app/src/mic_report.rs`: the `mic-check` report (`MicCheck`, `format_mic_check`, `mic_config_at`), moved out of `cli.rs` so the window shows the same text.
  - `crates/app/src/settings/{mod,form,keys,save,view}.rs`: the settings window. Only `view.rs` knows egui.
  - `crates/app/src/settings_process.rs`: the tray's launcher for the settings process.
- Findings from exploration that shape the plan:
  - B.1: the reader never sees a whole device block. `cpal_sink::render_into` renders a callback in `SCRATCH_FRAMES` (1024) chunks, and `mix_passthrough` reads in chunks of the same size. A 2048-frame callback is therefore two back-to-back 1024-frame reads. The block size has to be announced to the reader once per callback (`render_into` → `Mixer` → reader) before chunking. It cannot be inferred from read lengths.
  - B.1: the writer's high mark (target + 50 ms) uses the constant target. With a larger adaptive target it would drop audio, so the writer must read the reader's current target (an atomic).
  - B.1: 1.5 × the largest sink block alone is not enough. When capture blocks are as long as sink blocks (2048/2048) and the two clocks differ slightly, the order of capture and sink callbacks slips about every 3.5 minutes. When it slips, the fill measured before a read drops by one whole capture block. So the target must cover the sink block plus the capture block plus a margin. The writer records its largest capture block too (`fetch_max` on an atomic, realtime-safe).
  - B.2: `Backoff::failed` counts a failure that took at least 1.5 s as a stalled connect. A timed-out probe (2 s) must not count as one, because no thread leaked. After 8 such probes the process would wrongly stop using PulseAudio for good. A failed probe needs its own entry point that arms the hold-off without counting a stall.
  - B.2: `linux.rs` compiles only on Linux, and there is no CI. This machine can only type-check it through the stub-pkg-config clippy. So the probe/backoff decision becomes a pure function in `shared_host.rs`, tested on every OS. The saturated-listener test lives in `linux.rs` and runs only on a Linux host.
  - B.4: `CaptureStatus::Unavailable(String)` carries only the error text. The engine already has `BuildErrorKind`, so the status will carry the kind as well.
  - B.5: the device is chosen by `cpal_sink::first_match(devices, pattern, labels)` from `OutputTarget::Named(pattern)`, with `MicSettings::device_match` as the pattern. The default comes from the app's `platform::default_virtual_device()`.
  - Part A: the tray registers global hotkeys, which the OS hands to the tray before the settings window can see them. Re-assigning a combination the tray holds would therefore be impossible while it holds it. The tray unregisters its hotkeys while the settings process runs and registers them again (or reloads) when it exits.
  - Part A: a fresh probe knows no earlier physical input. So when the default input is the virtual device, the window (like `mic-check`) reports the refusal and says the tray's capture stays on the last physical input it used. The window cannot see the tray's live capture.
- Patterns to reuse:
  - The pure `SupervisorCore`-style state machines with injected effects.
  - The `HotkeyBackend` fake-trait pattern.
  - The recording `AudioControl`.
  - `TempDir` from the app's `test_support.rs`.
  - `write_atomically` and `create_temp` in `config.rs`.
  - The `#[cfg(any(target_os = "linux", test))]` pure-helper gate.
- Dependencies (workspace `Cargo.toml`, smallest feature set that builds on all three targets; versions chosen to build with `rust-version = 1.98` next to `tao`/`tray-icon`/`objc2 0.6`):
  - `eframe`: `default-features = false`, with `glow`, `default_fonts`, `wayland`, `x11`. No accesskit, no persistence. App crate only.
  - `rfd`: `default-features = false`, with `xdg-portal` plus the async-runtime feature it requires. This is pure Rust on Linux (no gtk, no pkg-config), so the stub-pkg-config cross-check keeps working. App crate only.
  - `socket2`: Linux target only, engine crate, for the bounded non-blocking AF_UNIX connect.
- Decisions:
  - Reload after the window closes: the tray snapshots the config file's bytes before launching the window and reloads only if they changed when the process exits. There is no exit-code protocol, and Cancel never reloads.
  - Paths of added sounds are stored as written by the file picker (absolute).
  - A hotkey without a modifier is accepted only for F1–F24, so a plain letter can never become a global hotkey. Escape alone clears the field.
  - The settings window owns only the `[[sound]]` entries' `name`, `path` and `hotkey`. `gain`, `[mic]`, `master_gain`, `stop_all_hotkey` and every comment are left as they are. `stop_all_hotkey` is read only to detect duplicates.
  - Interim tray status while the worker creates the device: `MicStatus::Starting` ("Virtual mic: starting").

## Development Approach

- **Testing approach**: Regular (code first, then tests in the same task).
- Complete each task fully before moving to the next.
- Everything in `CLAUDE.md` applies:
  - realtime rules and passthrough rules;
  - mixer handoff invariant;
  - platform code only in the per-OS files;
  - no `unwrap`/`expect` outside tests;
  - `thiserror` in the engine, `anyhow` in the app;
  - fakes for every test.
- No real audio from agents:
  - no `play`, no tray, no `mic-check` against real devices, no `pactl load-module`;
  - no automated step opens a window or spawns the settings process.
  - Device enumeration without a stream is allowed.
- After every task, all of these must be clean:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets` (zero warnings)
  - `cargo test --workspace`
  - `cargo check --workspace --target x86_64-pc-windows-msvc`
- Tasks that touch Linux code or add dependencies also run the stub-pkg-config Linux clippy from `CLAUDE.md`.
- **CRITICAL: every task MUST include new/updated tests**
- **CRITICAL: all tests must pass before starting next task**

## Implementation Steps

### Task 1: Short reason for an unavailable capture (B.4)

**Files:**
- Modify: `crates/engine/src/engine.rs`, `crates/app/src/actions.rs`, `crates/app/src/cli.rs` (if it matches on the variant)

- [x] `CaptureStatus::Unavailable` carries `{ kind: BuildErrorKind, error: String }`, filled by `mic_status_of` from `StreamStatus::Failed`.
- [x] `mic_status_label` gives a short reason for the kind. Wording can be tuned, but each kind gets its own text:
  - `FeedbackExcluded`: "default input is the virtual mic device";
  - `NoDevice`: "no input device";
  - `UnsupportedFormat` and `Backend`: "error, see log".
  - Example: "Virtual mic: BlackHole 2ch (active, microphone unavailable: no input device)".
- [x] Tests:
  - an engine test that `mic_status_of` carries the kind;
  - an app test of `mic_status_label` for every `CaptureStatus` and `MicStatus` variant, including each `BuildErrorKind`.
- [x] Run the verification gates. All must pass before Task 2.

### Task 2: macOS default pattern prefers BlackHole 2ch (B.5)

**Files:**
- Modify: `crates/engine/src/{engine.rs, cpal_sink.rs, test_support.rs}`, the `Backends` trait (`engine.rs`), `crates/app/src/{config.rs, hotkeys.rs, actions.rs, cli.rs}`, `crates/app/src/platform/{mod,macos,windows,linux}.rs`

- [x] Engine:
  - `MicSettings` gets `device_prefer: Option<String>`, which counts in `same_streams`.
  - `OutputTarget::Named` carries the pattern and the preference.
  - `first_match` becomes a pure `best_match`: among the devices the pattern names (unchanged `name_matches`), the first, in enumeration order, whose name or id also contains the preference (ignoring case); otherwise the first match.
  - Builds, identity queries and `probe` all use it. `Backends::mic_sink` takes the preference.
- [x] App:
  - `platform::default_virtual_device_preference()`: `Some("2ch")` on macOS, `None` elsewhere.
  - `MicConfig` gets the preference. It is set only when `virtual_device` is missing or blank (the platform default), so a user-set pattern keeps first-match semantics.
  - `settings_for` passes it on.
- [x] Wording:
  - the template comment, and the macOS `default_virtual_device` doc comment;
  - the `mic-check` "Virtual device pattern" line names the preference when there is one, for example `"BlackHole" (prefers a "2ch" device)`.
- [x] Tests:
  - `best_match` on ["BlackHole 16ch", "BlackHole 2ch"]: it picks the 2ch device with the preference and the 16ch device without one;
  - a preference no device matches falls back to the first match;
  - a blank pattern still matches nothing;
  - `resolve_mic` sets the preference only for a missing or blank `virtual_device`;
  - a changed preference restarts the mic streams (`same_streams`);
  - the `mic-check` line.
- [x] Run the verification gates. All must pass before Task 3.

### Task 3: Adaptive passthrough target and 10 ms macOS period (B.1)

**Files:**
- Modify: `crates/engine/src/passthrough.rs`, `crates/engine/src/mixer.rs`, `crates/engine/src/cpal_sink.rs`, `crates/engine/src/platform/macos.rs`

- [x] `platform/macos.rs`: `MIC_PERIOD_MS = Some(10)`, with the doc comment explaining why. A DAW sets the device buffer to 1024–4096 frames, and `mic_buffer_size` clamps the request to the device's range.
- [x] Block sizes:
  - `PassthroughShared` records the largest capture block written, in frames, with an atomic `fetch_max` in `write`. `begin_capture` resets it.
  - `PassthroughReader` records the largest sink block. A new `note_block(frames)` raises it, and `read` also raises it to `out.len()`. It is reset when the mixer is configured, since a new stream may have a new buffer size.
  - `Mixer` gets a realtime-safe `begin_callback(frames)` that forwards to the reader (a no-op without one). `cpal_sink::render_into` calls it with the whole callback's frame count before rendering chunks.
- [x] Effective target, in capture-rate samples:
  - `max(TARGET_FILL, sink block + capture block + BLOCK_MARGIN)`, with the sink block converted from the sink rate.
  - `BLOCK_MARGIN` must keep the target at exactly 30 ms for the existing tests' blocks (480 capture / 512 sink at 44.1–48 kHz); about 5 ms does. Tune it against the new tests.
  - It is computed before the priming check, so the first read of a long block already primes to the larger target.
  - The reader publishes it to an atomic in `PassthroughShared`.
- [x] The writer's high mark is `target + max(HIGH_MARK_MARGIN, capture block + BLOCK_MARGIN)`, using the published target, or `TARGET_FILL` before any is published.
- [x] The controller's error uses the effective target.
- [x] Realtime rules: only atomics and arithmetic are added; no allocation, no lock, no I/O.
- [x] Drift simulation:
  - `simulate` takes the capture and sink block sizes as parameters. The four existing tests keep 480/512 and their assertions unchanged.
  - The sink side calls `note_block(sink block)` and then reads in `mixer::SCRATCH_FRAMES` chunks, as production does.
- [x] New drift tests, each run with the existing three sink clocks for 10 simulated minutes:
  - 2048 capture / 2048 sink at 48k/48k;
  - 480 capture / 2048 sink at 48k/48k;
  - 2048 capture / 480 sink at 48k/48k;
  - 480 capture / 2048 sink at 44.1k → 48k.
  - Each must show:
    - zero underruns and zero overruns;
    - audible output;
    - fill within the effective target ± (20 ms + one capture block) after 5 s;
    - late mean correction within 20 ppm of the real clock ratio.
  - Note: 2048/2048 at ±200 ppm cannot meet the one-minute late mean. With equal block periods the fill seen before a read is a staircase that steps by one capture block at each slip (every ~3.5 min), so the controller corrects in a burst (~25 s at 0.2 %) per slip and sits near 0 in between. The reader only sees the callback order, not time, so no controller can spread that out. That test checks the mean correction from 60 s to the end instead: within 20 ppm plus one capture block over that span (~92 ppm), so a zero correction still fails. The other three keep the late 20 ppm check. The controller also steps once per announced callback rather than once per 1024-frame chunk, since the later chunks of a long callback see a lower fill and biased it by about 100 ppm.
- [x] Unit tests:
  - the target stays 30 ms for 512-frame blocks and grows for 2048-frame blocks;
  - `begin_capture` and `configure` reset the recorded block sizes;
  - the high mark follows the target;
  - `render_into` announces the whole callback size before chunking (a mixer with a reader primes and stays primed over a 2048-frame callback);
  - rendering with `begin_callback` allocates nothing (the existing allocation-check pattern).
- [x] Revert check: replace the effective target with `TARGET_FILL` (and the high mark with the old one) and confirm the 2048 tests fail and the four old ones pass. Then restore.
- [x] Run the verification gates. All must pass before Task 4.

### Task 4: Bounded PulseAudio probe connect and probe backoff (B.2)

**Files:**
- Modify: `crates/engine/src/platform/linux.rs`, `crates/engine/src/platform/shared_host.rs`, `crates/engine/Cargo.toml`, `Cargo.toml`

- [x] `server_answers` connects with a bounded non-blocking AF_UNIX connect (`socket2`, Linux target only):
  - an immediate `WouldBlock`/`EAGAIN` (the listener's backlog is full) fails the probe at once;
  - `EINPROGRESS` waits for writability for at most the probe timeout;
  - the socket is then checked as connected, switched back to blocking with the existing read and write timeouts, and turned into a `UnixStream`.
  - No `unsafe` unless `socket2` cannot express a step.
  - Note: no `unsafe`. socket2 has no public poll, so an `EINPROGRESS` (which a Unix socket connect does not normally report) is waited out by checking `take_error`/`peer_addr` every 5 ms. socket2's own `connect_timeout` was not used, because it treats `EAGAIN` as in progress. `libc` is a Linux dependency only for the `EINPROGRESS` constant (`io::ErrorKind::InProgress` is unstable).
- [x] `Backoff::probe_failed(now)`: arms the hold-off with the same doubling as `failed`, but never counts a stalled connect.
- [x] The decision in `connect_pulse` becomes a pure generic function in `shared_host.rs`, so it is tested on every OS: hold-off check, then probe, then connect, with `failed` on a connect error and `probe_failed` on a failed probe. Its arguments are the backoff, the clock reading and closures for the probe and the connect.
- [x] `connect_pulse` calls it and logs:
  - a failed probe at debug (the 1 Hz pollers would flood the log);
  - the existing warn and error lines for connects stay.
- [x] Tests in `shared_host.rs`, on every OS:
  - a failed probe arms the hold-off, and a call inside it neither probes nor connects;
  - the hold-off doubles up to 60 s over failed probes;
  - ten failed probes in a row leave `exhausted()` false;
  - a successful connect resets the hold-off;
  - the existing connect-failure behavior is unchanged.
- [x] Linux-only test in `linux.rs`: a `socket2` listener with backlog 0 that never accepts, filled with non-blocking connects until `WouldBlock`. Then `server_answers` must return false within the probe timeout plus slack.
  - This is deterministic on Linux, since a full backlog returns `EAGAIN` for a non-blocking connect.
  - It runs only on a Linux host. From macOS it is only type-checked by the stub-pkg-config clippy, and the task notes say so.
  - Note: this iteration also ran the engine's `platform::` tests in a local `rust:1.98` Linux container (no audio devices): all 20 passed. As a revert check, swapping the bounded connect for the old blocking `UnixStream::connect` made the backlog test fail.
- [x] Run the verification gates plus the Linux cross clippy. All must pass before Task 5.

### Task 5: Device worker thread for the virtual mic (B.3, part 1: state machine)

**Files:**
- Create: `crates/app/src/virtual_mic.rs`
- Modify: `crates/app/src/actions.rs` (move `WithVirtualMic`, `VirtualDevice`, `DEVICE_CHECK_INTERVAL` and their tests out), `crates/app/src/main.rs`

- [ ] Move `WithVirtualMic` and its tests into `virtual_mic.rs` unchanged first, and run the tests.
- [ ] Device worker: a thread that owns the device (`Option<G>`, `G: VirtualDevice + Send`) and the `create` function, and handles requests FIFO from a channel:
  - `Create { epoch }`: reuse the held device, or call `create`.
  - `Check { epoch }`: `is_present`.
  - `Release`: drop the device, which unloads it.
  - `Forget`: `mem::forget` it, for a shutdown whose stop timed out.
  - Every reply (`Created { epoch, result: Result<bool> }` where the bool says whether a device exists, `Checked { epoch, present }`, `Released`) goes to an injected `Fn(DeviceReply) + Send` callback.
  - `finish(timeout)` closes the channel and joins for at most the timeout. Three `pactl` calls at 3 s each bound the unload, so 10 s.
- [ ] `WithVirtualMic<C, R>` keeps the mic path state on the event loop thread and sends requests through a small `DeviceRequests` trait (the worker in production, a recording fake in tests). The device state is one of absent, creating (with an epoch), present or retired.
- [ ] Ordering invariants, unchanged in meaning:
  - The mic path starts only after a current `Created(Ok)` arrives.
  - Mic off stops the engine path (synchronously, bounded by the engine's 3 s stop timeout) before it sends `Release`. If `mic_stopped()` is false it keeps the device retired instead, and `release_retired` sends `Release` once the stream ended or takes the device back.
  - A vanished device (`Checked { present: false }`) stops the path, then sends `Release` and `Create`.
  - The lock and `pactl` code that never unloads another instance's sink is untouched.
- [ ] Stale replies:
  - every mic-on bumps the epoch, and a reply with an old epoch is ignored;
  - a mic-off while creating sends `Release` right away, which the FIFO runs after the pending create.
- [ ] `create_failed` logging (first failure at error, repeats at debug) and the `DEVICE_CHECK_INTERVAL` throttle move with the state machine. `restore_device` now sends `Check` or `Create` instead of calling them.
- [ ] `waiting_for_device` distinguishes "creating" (shown as `MicStatus::Starting`) from "create failed" (shown as not found).
- [ ] `shutdown(stop)`: stop the engine, then send `Release` (or `Forget` if a stream did not stop), then `finish`.
- [ ] Tests (fakes only, no `pactl`):
  - every existing `WithVirtualMic` test, adapted to deliver replies explicitly;
  - the path never starts before `Created(Ok)`;
  - `Created` arriving after the mic was turned off: no path start, and `Release` follows the create;
  - `Checked(false)` arriving after mic off is ignored;
  - `Created(Err)` arriving after mic off leaves no failure state;
  - on/off/on with replies in order starts the path once, with the latest settings;
  - retired-device handling with async releases;
  - the worker thread with a fake `create`: FIFO order, `Release` drops (a drop counter), `Forget` does not, and `finish` returns within its bound when a fake `create` blocks.
- [ ] Run the verification gates plus the Linux cross clippy. All must pass before Task 6.

### Task 6: Tray wiring for the device worker (B.3, part 2)

**Files:**
- Modify: `crates/app/src/tray_app.rs`, `crates/app/src/platform/mod.rs` (if `VirtualMic` needs a `Send` note)

- [ ] `UserEvent::Device(DeviceReply)`. The worker's reply callback sends it through an `EventLoopProxy`, and the loop hands it to `WithVirtualMic` and refreshes the status line.
- [ ] `TrayAudio` uses the worker. On Windows and macOS `create` returns `Ok(None)` at once, so the path starts after one round trip.
- [ ] `quit` uses the new `shutdown`.
- [ ] The tray's mic status maps "creating" to `MicStatus::Starting`.
- [ ] Tests:
  - a pure mapping test from the virtual-mic wait state to the tray's `MicStatus`;
  - the existing `mic_poll` tests still pass.
  - The event loop itself is not unit tested.
- [ ] Run the verification gates plus the Linux cross clippy. All must pass before Task 7.

### Task 7: Shared config editor and the sounds writer

**Files:**
- Modify: `crates/app/src/config.rs`
- Create: `crates/app/src/settings/mod.rs` (module declarations only for now), `crates/app/src/settings/save.rs`

- [ ] `config::update_config(path, edit: impl FnOnce(&mut DocumentMut) -> Result<()>)` does the whole sequence once:
  - canonicalize;
  - read;
  - parse to `DocumentMut`, leaving an invalid file untouched;
  - run `edit`;
  - `write_atomically`.
  - `set_mic_enabled` is rewritten on top of it, with no duplicated read, parse or write code.
- [ ] `settings/save.rs`, a pure `apply_sounds(doc, loaded: &[RawSound], rows: &[SoundRow]) -> Result<()>`:
  - It refuses with a clear error if the document's current `[[sound]]` entries no longer equal `loaded`, meaning the file's sounds changed since the window opened. A `[mic] enabled` toggle by the tray meanwhile is not a change to the sounds and is kept.
  - It rebuilds the array of tables in row order. A row from the file updates `name`, `path` and `hotkey` in its original table, keeping `gain`, unknown keys, comments and decor. No hotkey removes the key. A new row is a new table with `name`, `path` and optionally `hotkey`. Removed rows are dropped.
  - It creates the `[[sound]]` array if the file had none.
- [ ] `save_sounds(path, loaded, rows)` = `update_config` + `apply_sounds`.
- [ ] Tests:
  - round trip on `DEFAULT_CONFIG`: rename a sound, set and clear hotkeys, add one and remove one. Every comment, `[mic]`, `master_gain` and `stop_all_hotkey` stay byte-identical outside the edited entries, and the result parses with `config::parse` to the expected sounds;
  - a per-sound `gain` and an unknown key survive;
  - a concurrent `[mic] enabled` change is kept;
  - a changed sound list is refused and the file stays untouched;
  - an invalid file stays untouched;
  - `set_mic_enabled`'s existing tests still pass;
  - a save through a symlink writes the target (`TempDir`).
- [ ] Run the verification gates. All must pass before Task 8.

### Task 8: Settings form state, key mapping and validation (egui-free)

**Files:**
- Create: `crates/app/src/settings/form.rs`, `crates/app/src/settings/keys.rs`

- [ ] `keys.rs`, pure:
  - `config_key_name(ui_key: &str) -> Option<&'static str>` maps the UI library's key names (egui's `Key::name()` vocabulary, documented as such) to the config's key names: letters, digits, F1–F24, Space, Enter, Tab, Backspace, Delete, Insert, Home, End, PageUp, PageDown, arrows to Up/Down/Left/Right, numpad digits, and punctuation (Minus, Equal, Comma, Period, Slash, Semicolon, Quote, Backquote, BracketLeft, BracketRight, Backslash).
  - `Mods { cmd_or_ctrl, ctrl, alt, shift }`, where `ctrl` is a separate Control press on macOS only.
  - `hotkey_text(mods, key) -> KeyCapture` gives `Set(String)` in the config syntax (`CmdOrCtrl+Shift+1`, modifiers in a fixed order), `Clear` (Escape alone), or `Ignore` (unsupported key, a lone modifier, or a non-function key without a modifier).
  - Every `Set` result goes through `hotkeys::parse_hotkey`. A result it rejects becomes `Ignore`, so the window never writes a hotkey the tray rejects.
- [ ] `form.rs`, pure:
  - `SettingsForm` is built from config text: the parsed `RawConfig` gives the `loaded` sounds and `stop_all_hotkey`. A file that does not parse yields a form error, with editing and Save disabled.
  - Each `SoundRow` has `name`, `path`, `hotkey: Option<String>` and the index of its original entry (`None` for new rows).
  - Operations: `add(path)` (name pre-filled from the file stem), `remove(i)`, `set_name(i, ..)`, `set_hotkey(i, KeyCapture)`.
  - `validate()` returns per-row problems:
    - a blank name;
    - a hotkey from the file that `parse_hotkey` rejects;
    - a duplicate hotkey, compared by `HotKey::id()` between rows and against `stop_all_hotkey`, with every involved row marked.
  - `can_save()` is false while there is any problem or a form error.
  - `AUDIO_EXTENSIONS` (`wav`, `flac`, `ogg`, `mp3`) feeds the file picker filter.
- [ ] Tests:
  - the key mapping table: every supported name, and unsupported names;
  - modifier order and the CmdOrCtrl/Ctrl split;
  - Escape clears;
  - F5 alone is set and A alone is ignored;
  - every `Set` output parses through `parse_hotkey`;
  - adding from a path pre-fills the stem;
  - duplicates between two sounds, and with "stop all";
  - clearing one duplicate unblocks Save;
  - a blank name blocks Save;
  - a hotkey from the file that does not parse is marked;
  - a malformed file gives a form error.
- [ ] Run the verification gates. All must pass before Task 9.

### Task 9: Shared mic status report

**Files:**
- Create: `crates/app/src/mic_report.rs`
- Modify: `crates/app/src/cli.rs`

- [ ] Move `MicCheck`, `MicReport`, `format_mic_check`, `describe`, `access_text` and `mic_config_at` from `cli.rs` into `mic_report.rs` with their tests. `run_mic_check` uses them unchanged.
- [ ] The excluded-default line says the tray's capture stays on the last physical input it used, if there was one. This is the B.5/Part A wording.
- [ ] Add a `status_check(path) -> MicCheck` helper. It reads `[mic]` without creating the file, then reads `microphone_access` and `probe_mic`, for the window's background refresher.
- [ ] Tests: `format_mic_check` for every probe variant:
  - sink found, sink not found, sink with another error;
  - capture `None` (permission denied), capture running, capture with an excluded default and a fallback, `FeedbackExcluded`, `NoDevice`, another capture error;
  - each `MicrophoneAccess` value;
  - the preference line from Task 2.
- [ ] Run the verification gates. All must pass before Task 10.

### Task 10: `sound-babound settings` window (eframe + rfd)

**Files:**
- Modify: `Cargo.toml`, `crates/app/Cargo.toml`, `crates/app/src/cli.rs`, `crates/app/src/main.rs`, `crates/app/src/settings/mod.rs`
- Create: `crates/app/src/settings/view.rs`

- [ ] Add the `eframe` and `rfd` workspace dependencies with the minimal features from Context.
- [ ] CLI:
  - add the `settings` subcommand to clap (with a parse test);
  - `main` dispatches it to `settings::run()`, which returns `ExitCode`;
  - Windows keeps the GUI subsystem, so no console window opens.
- [ ] `settings::run()`:
  - resolve the config path with `paths::ensure_default_config` and build the form;
  - start a status thread that calls `mic_report::status_check` every 1 s and publishes the formatted report (a `Mutex<Option<String>>` plus a `request_repaint` on the egui context). The probe never runs on the UI thread, since a PulseAudio query can block;
  - run `eframe::run_native` with the "Sound-Babound settings" title and a fixed starting size, and request focus once at start (the macOS bundle is an `LSUIElement` agent).
- [ ] `view.rs` is thin and holds no logic of its own:
  - the status section with the report text, or "checking…";
  - the sounds list: an editable name, the path as text, the hotkey field and Remove;
  - "Add sound…" opens `rfd::FileDialog` with the `AUDIO_EXTENSIONS` filter;
  - a focused hotkey field captures the next key event (egui `Key::name()` plus modifiers) through `keys::hotkey_text`;
  - problems from `validate()` show next to their rows;
  - Save is disabled unless `can_save()`. It calls `save::save_sounds`; on success it closes the window, and on error it shows the message and stays open;
  - Cancel and the window's close button close without writing.
- [ ] One test in `view.rs` that opens no window: for every `egui::Key::ALL`, `config_key_name(key.name())` is either a name `parse_hotkey` accepts or the key is in the documented unsupported list. This catches egui renames.
- [ ] Run the verification gates plus the Linux cross clippy (eframe and rfd must type-check for `x86_64-unknown-linux-gnu` with the stub pkg-config). All must pass before Task 11.

### Task 11: Tray "Settings…" item, one instance, pause hotkeys, reload after save

**Files:**
- Create: `crates/app/src/settings_process.rs`
- Modify: `crates/app/src/actions.rs` (`Action::OpenSettings`, `MENU_SETTINGS`), `crates/app/src/tray_app.rs`

- [ ] `SettingsSpawner` trait: `spawn(&mut self) -> Result<()>`.
  - The production spawner runs `std::env::current_exe()` with `settings`, with stdin null and stdout/stderr inherited.
  - A waiter thread waits on the child and sends `UserEvent::SettingsClosed` through the proxy.
- [ ] Pure controller `SettingsLauncher<S>`:
  - `open(snapshot)` returns `Launched`, `AlreadyOpen` or `Failed(err)`. The snapshot is the config file's bytes, read before the spawn.
  - `closed(current_bytes) -> bool` says whether to reload: true only if the bytes changed.
  - While a child runs, `open` spawns nothing.
- [ ] Tray:
  - the menu gets "Settings…" above "Reload config";
  - on `Launched`, unregister all hotkeys;
  - on `SettingsClosed`, reload through the existing path if the controller says so; otherwise, or if the reload failed, register the current board's hotkeys again (`Hotkeys::replace`);
  - `AlreadyOpen` is logged at info;
  - quitting the tray leaves an open settings window running.
- [ ] Tests:
  - with a fake spawner: one instance (a second `open` while running spawns nothing);
  - a failed spawn leaves the launcher closed and reopenable;
  - an unchanged file means no reload and a changed file means reload;
  - a missing file before and after means no reload;
  - after close, `open` launches again;
  - the menu-id mapping for `MENU_SETTINGS`;
  - with the fake `HotkeyBackend`: hotkeys are unregistered while the window is open and registered again after a close without changes.
- [ ] Run the verification gates. All must pass before Task 12.

### Task 12: Verify acceptance criteria

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets`, with zero warnings
- [ ] `cargo test --workspace`
- [ ] `cargo check --workspace --target x86_64-pc-windows-msvc`
- [ ] Linux stub pkg-config `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu`, which also covers eframe and rfd.
- [ ] Revert checks, each restored afterwards:
  - B.1: a fixed 30 ms target fails the 2048-block drift tests and passes the four old ones;
  - B.2: removing the `probe_failed` call fails the pure backoff test;
  - B.4: the generic label fails the per-kind label test;
  - B.5: `first_match` instead of `best_match` fails the 16ch-before-2ch test.
- [ ] Confirm that the tests for AC2 (config round trip, keys, duplicates, status text) and AC3 (launcher and hotkey pause) exist and pass.
- [ ] Coverage of the new pure modules (`settings/{form,keys,save}.rs`, `settings_process.rs`, `virtual_mic.rs`, `mic_report.rs`, the passthrough changes): aim for 80% or more, measured as in stage 2. `view.rs` and the eframe entry are excluded.

### Task 13: Update documentation

- [ ] README:
  - the settings window: the tray item, `sound-babound settings`, what it shows and edits, that hotkeys pause while it is open, Save and Cancel, and comments kept;
  - the macOS default (BlackHole 2ch preferred, any BlackHole otherwise, a set pattern matches first);
  - the macOS 10 ms mic period, and that larger device buffers still work at a higher latency;
  - the clearer "microphone unavailable" reasons.
- [ ] Config template: the `virtual_device` comment describes the macOS preference, and the header mentions "Settings…" for sounds and hotkeys.
- [ ] `CLAUDE.md`:
  - module list: `settings/` (only `view.rs` knows egui), `settings_process.rs`, `virtual_mic.rs` with the device worker and its invariants, `mic_report.rs`, `config::update_config`;
  - the CLI's `settings` subcommand;
  - passthrough rules: the adaptive target (block sizes announced per callback, the formula, the high mark that follows it) and the new drift tests;
  - `MIC_PERIOD_MS` on macOS;
  - the probe's bounded connect and the probe backoff rule (it arms the hold-off without counting a stall);
  - `CaptureStatus::Unavailable` with its kind;
  - `best_match` and the preference;
  - eframe, rfd and socket2 in the dependency notes;
  - "no window opened by an automated run" in the no-real-audio rule.

## Post-Completion

These checks are manual and done by a human:

- Settings window, on the dev Mac with the bundle (`scripts/bundle-macos.sh`, launched from Finder):
  - open "Settings…" from the tray and see the window come to the front with the virtual-device status updating every second;
  - click "Settings…" again and see no second window;
  - add a sound through the file picker (check the name pre-fill), set a hotkey by pressing it, try a duplicate (marked, Save disabled), clear one with Escape, and Save;
  - check that the config file kept its comments and that the tray reloaded (the new menu item, and the new hotkey plays);
  - confirm that the tray hotkeys do nothing while the window is open and work again after Cancel.
- Zoom check with BlackHole 2ch and BlackHole 16ch both installed:
  - with the default pattern, `mic-check` and the window name BlackHole 2ch;
  - set BlackHole's buffer to 2048 frames in Audio MIDI Setup (or a DAW): call participants still hear your voice and the sounds;
  - set the default input to BlackHole and see the tray's "microphone unavailable: default input is the virtual mic device", or the fallback.
- Windows: the settings window opens from the tray without a console window.
- Linux:
  - the window opens on X11 and on Wayland, and the file picker (xdg portal) works;
  - with the sound server stopped (`SIGSTOP`) and the mic on, the tray stays responsive (the menu opens, Quit returns within about 10 s);
  - turning the mic off and on during the stop does not freeze it;
  - after `SIGCONT` the device comes back;
  - run the Linux-only saturated-listener probe test (`cargo test -p babound-engine`) on a Linux host.
