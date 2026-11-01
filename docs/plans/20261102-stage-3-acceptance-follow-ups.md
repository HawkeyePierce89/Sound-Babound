# Stage 3 acceptance follow-ups (ten minor findings)

## Overview

This plan closes the ten minor findings from the stage 3 acceptance review (PR #3, `42074f5`). It is a cleanup with the smallest diff that closes each one, grouped by area:

- **Passthrough** (`passthrough.rs`):
  - a fallback target for the writer before the reader publishes one, so the first two capture blocks never trim;
  - a re-prime at every `set_sink_rate`, keeping the learned correction when the rate is the same;
  - two new drift cases;
  - a doc note on `CorrectionCheck::Settled`.
- **PulseAudio probe** (`platform/linux.rs`, `platform/shared_host.rs`):
  - one deadline for the whole probe;
  - a 10 s ceiling on a hold-off made only of failed probes.
- **Virtual mic worker** (`virtual_mic.rs`):
  - a vanished device goes through the retire decision;
  - a test for a stale-epoch `Checked(false)`;
  - an honest comment and log for `WORKER_FINISH_TIMEOUT`.
- **Docs**: CLAUDE.md and README.

## Context

- Files involved:
  - `crates/engine/src/passthrough.rs`: `PassthroughShared::write`, `high_mark_samples`, `PassthroughReader::set_sink_rate`, the drift test harness (`simulate`, `CorrectionCheck`, `assert_holds_with_long_blocks`).
  - `crates/engine/src/mixer.rs`: the test `configure_passes_the_sink_rate_to_the_reader` asserts that the same rate keeps the reader primed, so it changes with item 2.
  - `crates/engine/src/platform/linux.rs`: `connect_pulse` (doc), `PROBE_TIMEOUT`, `server_answers`, `connect_within`, `wait_connected`, `roundtrip`, and the probe tests (Linux only).
  - `crates/engine/src/platform/shared_host.rs`: `Backoff` (`failed`, `probe_failed`, `hold_off`, `succeeded`), `try_connect`, the module doc, and the `try_connect` tests (they run on every OS).
  - `crates/app/src/virtual_mic.rs`:
    - the device worker parts: `WORKER_FINISH_TIMEOUT`, `DeviceWorker::finish_within` and `DeviceState::Retired`;
    - the `WithVirtualMic` methods: `checked`, `drop_device`, `release_retired`, `set_mic`, `shutdown` and `has_retired_device`;
    - the test module, with the recording `FakeRequests` and the `with_slow_stop` helper.
  - `CLAUDE.md`, `README.md`.
- Related patterns:
  - The pure helpers in `passthrough.rs` (`samples_for`, `target_samples`, `effective_target`), which are integer-only and realtime-safe.
  - The fake-clock `attempt` and `Clock` helpers in the `shared_host.rs` tests.
  - The saturated-listener and the stall-after-`Auth` tests in `linux.rs` (a `UnixListener` server thread, timing asserts).
  - The `with_slow_stop` helper and the retired-device tests in `virtual_mic.rs`.
- Dependencies: none new.

## Development Approach

- **Testing approach**: TDD for the behavior changes (items 1, 2, 5, 6, 7): write the test that shows the finding, see it fail, then fix. The test-only and doc-only items (3, 4, 8, 9, 10, 11) are written directly.
- Complete each task fully before moving to the next.
- Keep every fix minimal. No tuning of `KP`, `KI`, `MAX_CORRECTION`, `TARGET_FILL` or `HIGH_MARK_MARGIN`. No change to `WORKER_FINISH_TIMEOUT`'s value. No loosening of any existing drift tolerance.
- Realtime rules hold in everything `PassthroughShared::write` and `PassthroughReader::read` reach: integer math, no allocation, no lock, no I/O, no tracing.
- No `unwrap`/`expect` outside tests. Platform code stays in the per-OS files.
- No real audio: never run `sound-babound play`, the tray, `mic-check`, `settings` or `pactl load-module`.
- `linux.rs` compiles only on Linux, and there is no CI. From macOS it is type-checked by the stub-`PKG_CONFIG` clippy and its tests are run in a local `rust:1.98` Docker container, as stage 3 did. If Docker is unavailable, say so in the task notes.
- **CRITICAL: every task MUST include new/updated tests**
- **CRITICAL: all tests must pass before starting next task**

## Implementation Steps

### Task 1: Passthrough: fallback target, same-rate re-prime, drift coverage

**Files:**
- Modify: `crates/engine/src/passthrough.rs`
- Modify: `crates/engine/src/mixer.rs` (test only)

Item 1, fallback target:

- [x] Add a pure, integer-only helper `fallback_target(rate, capture_block)`. It returns `max(target_samples(rate), capture_block + samples_for(rate, BLOCK_MARGIN))`, at most `MAX_TARGET`.
- [x] In `write`, use the helper in place of `target_samples(rate)` when no target is published (`target == 0`). It sets both the high mark and the trim-back level. Invariant: with a fallback target `T ≥ B + M`, the high mark `T + max(H, B + M)` is at least `2B + 2M`, so two writes of `B` frames never trim. Once the reader publishes a target, it stands as now.
- [x] Update the docs of `write` and of the module ("the published target, or the fallback target before the reader published one"). Update the "Before any target is published" comment in `the_high_mark_follows_the_published_target` if it describes the write path.
- [x] Tests:
  - on a fresh capture at 48 kHz, write two 2048-sample blocks with no read: `overruns() == 0` and `fill() == 4096`;
  - the same with two 4096-sample blocks: `fill() == 8192`;
  - unit asserts on `fallback_target`: 1440 for 480 frames, 2288 for 2048, 4336 for 4096;
  - the existing 480-block high-mark/trim test still passes unchanged, because its fallback stays at 1440.

Item 2, same-rate re-prime:

- [x] `set_sink_rate` drops `primed` at every call. When the rate is unchanged, it keeps `correction`, `integral` and `fill_error`, so the learned clock ratio survives; re-priming sets `fresh`, which only re-seeds the EMA. A rate change still calls `reset()`. Update the method doc.
- [x] Test: prime with 512-frame reads at 48k/48k while 480-frame capture blocks are written; note the correction; call `set_sink_rate(48_000)`; `note_block(2048)` and read 2048 frames; assert no underrun, the correction kept, and `!is_primed()` while the fill is below the new target; write until the fill reaches the published target, read again, and assert `is_primed()` and still zero underruns.
- [x] Update `a_sink_rate_change_resets` (same rate now re-primes but keeps the correction) and `mixer.rs::configure_passes_the_sink_rate_to_the_reader` ("the same rate re-primes").

Item 3, drift test gaps:

- [x] Add `drift_with_4096_frame_blocks_holds_the_target`: 48k/48k, 4096/4096, `SINK_CLOCKS`, `CorrectionCheck::Settled`. Here `capture block + BLOCK_MARGIN` (90.3 ms) is more than `HIGH_MARK_MARGIN` (50 ms), so the high mark's capture-block branch is exercised.
- [x] Add `drift_48k_to_44k1_with_a_2048_frame_sink_block_holds_the_target`: capture 48k with 480 frames, sink 44.1k with 2048 frames, sink clocks `SINK_CLOCKS × 44100/48000`, `CorrectionCheck::Late`.
- [x] Both use the existing long-block tolerances, unchanged. If either case fails, stop and report the numbers. Do not loosen a tolerance or tune a constant.

Item 4, the `Settled` note:

- [x] Extend the `CorrectionCheck::Settled` doc comment, with no check changes:
  - for a 2048-frame capture block at 48 kHz, the tolerance is 20 ppm plus one capture block over the 540 s span, about 99 ppm;
  - the ±200 ppm sink clocks prove the controller alive: without a correction the fill would drift 9.6 samples/s, past one block in about 3.5 minutes, and the deviation and overrun/underrun asserts would fail;
  - the 48000.5 Hz variant (about 10 ppm, about 300 samples in 10 minutes) passes even with a dead controller; it only proves the controller harmless: no runaway, no underrun or overrun, the fill inside the bound.
- [x] Run `cargo test -p babound-engine`; it must pass before Task 2.

### Task 2: PulseAudio probe: one deadline, and a 10 s ceiling for a probe-only hold-off

**Files:**
- Modify: `crates/engine/src/platform/linux.rs`
- Modify: `crates/engine/src/platform/shared_host.rs`

Item 5, one deadline:

- [ ] `server_answers` computes `deadline = Instant::now() + timeout` once. `connect_within` and `wait_connected` take the deadline; they wait out an `EINPROGRESS` until the deadline and no longer set fixed socket timeouts.
- [ ] All handshake I/O goes through a small wrapper over `&UnixStream` that implements `Read` and `Write`, used by the `BufReader` and by `roundtrip`'s writer:
  - before every call, it sets the socket's read or write timeout to the time left until the deadline;
  - with no time left, it fails with `io::ErrorKind::TimedOut`, since a zero timeout is not allowed.
  The bound then holds even against a server that trickles bytes. A timeout still maps to `Answer::Silent`. Result: a probe never takes longer than `PROBE_TIMEOUT` plus one `CONNECT_POLL`.
- [ ] Update the docs:
  - `PROBE_TIMEOUT`: the whole probe's deadline;
  - `connect_within` and `server_answers`: one deadline over the connect and both messages;
  - `connect_pulse`: "blocks every mic caller behind that lock for at most the probe's deadline".
- [ ] Linux-only test `a_server_that_answers_slowly_fails_the_probe_within_one_deadline`: the fake server reads `Auth`, waits about 70 % of a 500 ms timeout, replies to `Auth`, then reads `SetClientName` and never answers; assert `!server_answers(...)` and an elapsed time below the timeout plus 300 ms (before the fix it takes about 1.7× the timeout).
- [ ] Linux-only test with a server that trickles the `Auth` reply one byte at a time, below the timeout per byte: assert it is bounded the same way.
- [ ] Keep the existing never-answers, stall-after-`Auth` and saturated-backlog tests passing.

Item 6, the probe hold-off ceiling:

- [ ] Add `PROBE_HOLD_OFF_MAX: Duration = 10 s`.
- [ ] `Backoff` gets a `connect_failed: bool` field: the current streak includes a failed connect. `failed` sets it and `succeeded` clears it.
- [ ] `hold_off(now, cap)`: `probe_failed` passes `PROBE_HOLD_OFF_MAX`, or `BACKOFF_MAX` when `connect_failed` is set, so a probe failure after a failed (stalled) connect keeps the full doubling and never shortens the connect's hold-off; `failed` always passes `BACKOFF_MAX`.
- [ ] The shared failure count, `STALL_THRESHOLD` and `MAX_STALLED_CONNECTS` are unchanged.
- [ ] Update the docs of `Backoff`, `probe_failed`, `try_connect`, the `shared_host.rs` module and `connect_pulse` (no longer "doubling up to a minute" for probes).
- [ ] `try_connect` tests (every OS, fake clock):
  - a probe-only streak gives 2, 4, 8, 10, 10 s; update `the_hold_off_doubles_over_failed_probes_up_to_its_cap` to the 10 s cap;
  - a connect-failure streak still reaches and holds 60 s;
  - a mixed streak, a stalled connect then failed probes, gives 4, 8, 16, 32, 60 s (never capped at 10);
  - a success resets the cap to the probe ceiling;
  - `failed_probes_never_exhaust_the_backoff` still passes.
- [ ] Run `cargo test -p babound-engine` and the Linux cross clippy with the stub `PKG_CONFIG`.
- [ ] If Docker is available, run the engine's `platform::` tests in a local `rust:1.98` container. Install `libasound2-dev` and `pkg-config`, and use a separate `CARGO_TARGET_DIR` (no audio device is needed). Record the result in the task notes.
- [ ] Everything must pass before Task 3.

### Task 3: Virtual mic worker: retire a vanished device, the stale-epoch test, the finish timeout

**Files:**
- Modify: `crates/app/src/virtual_mic.rs`

Item 7, retire a vanished device:

- [ ] `DeviceState::Retired` becomes `Retired { vanished: bool }`. `drop_device` retires with `vanished: false`. `has_retired_device` and `shutdown` match `Retired { .. }`.
- [ ] In `checked`, after `stop_path`: if `mic_stopped()`, it sends `Release` and then `begin_create()` (as today); otherwise it warns and sets `Retired { vanished: true }`, sending nothing.
- [ ] In `release_retired`, once `mic_stopped()`, a vanished device sends `Release`, then `begin_create()` (a new epoch) if the mic is on, and otherwise goes to `Absent`. It is never taken back.
- [ ] In `set_mic(Some)` on `Retired { vanished: true }` with the stream ended, it sends `Release` and then `begin_create()` instead of reusing the device. While the stream runs, it waits as today.
- [ ] Update the docs of `checked`, `release_retired` and `DeviceState`.
- [ ] Tests (recording `FakeRequests`, `with_slow_stop`-style control):
  - `Checked(false)` with `mic_stopped` false: the path stops, `has_retired_device()` is true, and no request is sent;
  - a later `release_retired` with `mic_stopped` true sends `Release`, then `Create { epoch: n+1 }`; a `Created(Ok)` for it starts the path;
  - `Checked(false)` with `mic_stopped` true still sends `Release` then `Create` at once (today's behavior; keep or extend the existing test);
  - a vanished, retired device with the mic turned off: once the stream ends, `release_retired` sends only `Release`.

Item 8, stale-epoch test:

- [ ] Add a test that runs mic on, `Created(Ok)` (epoch 1), mic off, mic on again (`Create` epoch 2), then `Checked { epoch: 1, present: false }`. Assert no extra `set_mic(None)`, no `Release` and no second `Create` after epoch 2's.

Item 9, `WORKER_FINISH_TIMEOUT`:

- [ ] Rewrite the comment. The 10 s timeout bounds how long quit and mic-off may freeze the event loop. It deliberately does not cover the worst case: a create is `list`, up to two `unload`s and two `load`s at 3 s each, about 15 to 18 s, and a `Release` queued behind it brings that to about 27 s. The comment also says what happens when it fires: the worker is left running, the device may stay loaded, and the next start unloads it as stale.
- [ ] `finish_within`'s warning says the same in one sentence. The value stays 10 s.
- [ ] Run `cargo test -p sound-babound`; it must pass before Task 4.

### Task 4: Verify acceptance criteria

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets` with zero warnings
- [ ] `cargo test --workspace`. The existing drift tests pass with unchanged tolerances, and the new cases (4096/4096, 48k → 44.1k long block, two writes with no read, same-rate rebuild) pass.
- [ ] `cargo check --workspace --target x86_64-pc-windows-msvc`
- [ ] The Linux cross-check: `PKG_CONFIG=<stub> PKG_CONFIG_ALLOW_CROSS=1 cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu`
- [ ] Every new behavior has a test that fails without its fix: items 1, 2, 5, 6 and 7.

### Task 5: Update documentation

**Files:**
- Modify: `CLAUDE.md`
- Modify: `README.md`

`CLAUDE.md`:

- [ ] Passthrough rules: replace "Past the high mark (target + 50 ms)" with the published target plus `max(HIGH_MARK_MARGIN, capture block + BLOCK_MARGIN)`.
- [ ] Adaptive target: say that before a target is published the writer uses `max(TARGET_FILL, capture block + BLOCK_MARGIN)`, so the first two capture blocks never trim.
- [ ] Say that `set_sink_rate` re-primes at every call and keeps the controller when the rate is the same.
- [ ] List the new drift cases in the drift-test sentence.
- [ ] `platform/` bullet:
  - the probe has one deadline over the connect and both messages (at most `PROBE_TIMEOUT` plus one poll);
  - a hold-off made only of failed probes is capped at `PROBE_HOLD_OFF_MAX` (10 s), while a streak with a failed connect keeps doubling to 60 s.
- [ ] `virtual_mic.rs` bullet:
  - a `Checked(false)` goes through the retire decision (a vanished, retired device is released and created again once the stream ends);
  - `WORKER_FINISH_TIMEOUT` bounds quit and mic-off rather than covering the worst case, and the comment says what a timeout leaves behind.

`README.md`:

- [ ] In the config section, where the two cases are listed, add one sentence: the app also creates a hidden `.config.toml.lock` file next to the config (the edit lock) and never removes it.
- [ ] Re-read both files against the code for every mechanism touched: high mark, fallback target, re-prime, probe deadline and hold-off, `checked` retire, lock file, worker timeout.

## Post-Completion

Manual checks, done by a human:

- Run `cargo test -p babound-engine` on a Linux host, so the Linux-only probe tests (slow-`Auth`, trickle, saturated backlog) run natively, unless the Docker run in Task 2 already covered them.
- Optional: on a machine with a DAW at 2048 or 4096 frames, start the tray with the mic on and confirm the log shows no passthrough overrun at startup.
