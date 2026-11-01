# Built-in sound set

## Overview

Replace the single embedded demo chime with a registry of built-in sounds, and embed a curated CC0 set of twelve classic call-soundboard sounds as OGG Vorbis. A reproducible script converts the hand-staged Freesound files and writes `SOURCES.md` with the credits. The default config lists every built-in sound with no hotkey. The settings window gets an "Add built-in…" menu and a per-row Play/Stop audition, which runs `sound-babound play --output-only` in a child process on the default output only, never into the virtual mic. `play` resolves `builtin:<name>` through the same resolver as the config.

## Context

- Files involved:
  - Create: `scripts/convert-sounds.sh`, `crates/app/assets/sounds/<name>.ogg` (12 files), `crates/app/assets/sounds/SOURCES.md`, `crates/app/src/settings/audition.rs`
  - Modify: `crates/app/src/config.rs`, `crates/app/src/cli.rs`, `crates/app/src/main.rs`, `crates/app/src/hotkeys.rs` (tests), `crates/app/src/actions.rs` (tests), `crates/app/src/settings/form.rs`, `crates/app/src/settings/save.rs` (tests), `crates/app/src/settings/mod.rs`, `crates/app/src/settings/view.rs`, `README.md`, `CLAUDE.md`
- Source material: `~/Downloads/babound-sounds/` holds 12 files plus `sources.txt`. Each line of `sources.txt` reads `<name> | <url> | <title> | <uploader> | <license> | <start s> | <end s>`. The title column was added by hand (decided in Q&A), and start/end are optional. All 12 lines are currently CC0 with titles. `crickets` keeps 0–6 s and `bell` keeps 10.8–14.2 s.
- Related patterns:
  - `gen_demo_wav` (a reproducible asset generator, documented in CLAUDE.md).
  - `settings_process.rs`: the `SettingsSpawner` trait plus `ProcessSpawner`, an injected `command: fn() -> Result<Command>`, and tests that run the test binary with `--list`.
  - `settings/mod.rs`: the status thread that wakes the UI with `request_repaint`.
  - `SoundSource::resolve`, plus the existing tests `builtin_demo_decodes` and `resolve_rejects_empty_and_unknown_builtin`.
  - The `hotkeys.rs` test that expects `builtin:nope` in a warning.
- Dependencies: no new crates. The script needs `ffmpeg`, `ffprobe` and `oggenc`. Decided in Q&A: ffmpeg does all the processing and pipes raw PCM to `oggenc` from vorbis-tools.
- Prerequisite: `oggenc` is not installed on this machine (`brew install vorbis-tools`). It must be present before Task 1 runs. If it is missing, Task 1 stops and reports it. The agent does not install it.

## Development Approach

- **Testing approach**: Regular (code first, then tests).
- Complete each task fully before moving to the next.
- No real audio from agents. The sounds are verified only by decoding them in tests. Never run `sound-babound play`, the tray, `mic-check`, `sound-babound settings`, or `pactl load-module`, and never click Play in the window. Audition tests use a fake spawner. The real spawner is tested with the test binary and `--list`.
- The registry replaces the demo plumbing: `BUILTIN_DEMO`, `DEMO_WAV`, `DEMO_LABEL` and `SoundSource::BuiltinDemo` are removed, and every user moves to the registry. Nothing sits beside the old code.
- No `unwrap`/`expect` outside tests, `anyhow` in the app, and no `cfg(target_os)` in shared code.
- After every task, run the verification set from CLAUDE.md: fmt check, clippy with zero warnings, `cargo test --workspace`, and the Windows `cargo check`. The Linux clippy cross-check with the stub `PKG_CONFIG` runs at the end.
- **CRITICAL: every task MUST include new/updated tests**
- **CRITICAL: all tests must pass before starting next task**

## Implementation Steps

### Task 1: Conversion script, assets and SOURCES.md

**Files:**
- Create: `scripts/convert-sounds.sh`
- Create: `crates/app/assets/sounds/*.ogg`, `crates/app/assets/sounds/SOURCES.md`

- [x] Write `scripts/convert-sounds.sh <staging-dir>` in bash (`set -euo pipefail`), with these rules:
  - It resolves the repo root from the script's location.
  - It checks for `ffmpeg`, `ffprobe` and `oggenc`. If one is missing, it stops with an install hint (`brew install ffmpeg vorbis-tools`, or the distro packages).
  - It only reads the staging dir and writes only under `crates/app/assets/sounds/`. It uses no temp files: every intermediate result goes through pipes.
- [x] Parse `sources.txt`:
  - Skip `#` comments and blank lines, split on `|` and trim each field.
  - Keep a line only if its license is `CC0` and its URL, title and uploader are all non-empty. A file in the folder without such a line, or a line without a file, is reported on stderr as skipped and never embedded.
  - Names must match `^[a-z0-9-]+$`.
  - Lines are processed in `sources.txt` order.
- [x] Per sound, pass 1 (analysis). Read the source sample rate with `ffprobe`; the rate is the source rate, or 48000 if the source is above 48 kHz. Then run `ffmpeg` with:
  - input seeking `-ss <start> -t <end-start>` placed before `-i`, when a window is given;
  - `-ac 1 -ar <rate>`;
  - the filter chain `silenceremove=start_periods=1:start_threshold=-50dB,areverse,silenceremove=start_periods=1:start_threshold=-50dB,areverse,atrim=end=6,volumedetect`;
  - output as `-f f32le -` piped into `wc -c`, to get the exact frame count.

  The script parses `max_volume` from ffmpeg's stderr and computes the gain as `-1 - max_volume` dB with `awk`.
- [x] Per sound, pass 2 (encode). Use the same input options and chain without `volumedetect`, plus `volume=<gain>dB,afade=t=out:st=<duration-0.02>:d=0.02` (the fade is always at the end, which is the cut for capped sounds). Output `-f s16le -` and pipe it into `oggenc -Q -r -B 16 -C 1 -R <rate> -q 4 --serial <fixed per-sound number, e.g. its line index> -o crates/app/assets/sounds/<name>.ogg -`. Delete any other `*.ogg` in that directory, so the set is exactly the accepted lines.
- [x] Generate `SOURCES.md`:
  - a short header: CC0 1.0 credits for the embedded sounds, generated by the script, do not edit by hand. It also says that `builtin:demo` is not listed because it is generated by `gen_demo_wav`.
  - then one Markdown table row per accepted sound: ``| `builtin:<name>` | <title> | <uploader> | <url> | CC0 1.0 |``, with `|` in titles escaped and no window columns.
- [x] Run it on `~/Downloads/babound-sounds`, then run it again and check that `git status --porcelain crates/app/assets/sounds` shows no change after the second run (the files are untracked until committed, so compare checksums of both runs). Also run `bash -n` on the script, and `shellcheck` if it is installed.
- [x] Check the total size: the `.ogg` files plus `demo.wav` (132 KB) must be under 600 000 bytes. If they are not, stop and report the sizes instead of loosening the budget or the quality.

### Task 2: The built-in registry and resolution

**Files:**
- Modify: `crates/app/src/config.rs`, `crates/app/src/cli.rs`, `crates/app/src/hotkeys.rs`, `crates/app/src/actions.rs`

- [x] Add `pub struct BuiltinSound { name, path, label, bytes }`, all `&'static`:
  - `path` is `"builtin:<name>"` and is also the decode label.
  - `Debug` and `PartialEq` are written by hand, by name only, so the bytes never print or get compared.
  - A small `macro_rules!` builds an entry with `concat!`/`include_bytes!("../assets/sounds/<name>.ogg")`.
- [x] Add `pub static BUILTIN_SOUNDS: &[BuiltinSound]`. It starts with `demo` ("Demo chime", `../assets/demo.wav`, unchanged), followed by the twelve in `sources.txt` order with these labels: "Air horn", "Drum roll", "Crickets", "Applause", "Rimshot", "Sad trombone", "Bell", "Buzzer", "Ding", "Boing", "Laugh", "Slide whistle". Also add `pub fn builtin(name) -> Option<&'static BuiltinSound>` (case-insensitive) and a `DEMO: &BuiltinSound` accessor for the `play` default.
- [x] Replace `SoundSource::BuiltinDemo` with `SoundSource::Builtin(&'static BuiltinSound)`:
  - `resolve` matches the `builtin:` prefix case-insensitively and looks up the name case-insensitively.
  - An unknown name fails with `unknown built-in sound "builtin:x"; available: builtin:demo, builtin:airhorn, …`.
  - `load` decodes `entry.bytes` with `entry.path` as the label.
  - Remove `BUILTIN_DEMO`, `DEMO_WAV` and `DEMO_LABEL`, and update their users (the `cli.rs` default and its tests, the `hotkeys.rs` and `actions.rs` tests) to the registry.
- [x] Tests in `config.rs`:
  - every entry decodes with a non-zero frame count;
  - names are unique and match `[a-z0-9-]+`;
  - labels are non-empty;
  - every entry lasts at most 6 s (frames / rate);
  - the total `bytes.len()` of the registry is under 600 000 (the budget test);
  - the `builtin:` names in `SOURCES.md` (read from `CARGO_MANIFEST_DIR`) equal the registry minus `demo`, in both directions;
  - every `*.ogg` in `assets/sounds/` is a registry entry, so nothing unlisted ships;
  - resolve is case-insensitive (`BUILTIN:AirHorn`);
  - an unknown name lists the available names.

  The `hotkeys.rs` test expecting `builtin:nope` in a warning must still pass.
- [x] Run the test suite; it must pass before Task 3.

### Task 3: The default template lists the built-in set

**Files:**
- Modify: `crates/app/src/config.rs`, `crates/app/src/hotkeys.rs`, `crates/app/src/actions.rs`, `crates/app/src/settings/form.rs`, `crates/app/src/settings/save.rs`

- [x] In `DEFAULT_CONFIG`, replace the demo entry with one `[[sound]]` per registry entry except the demo, in registry order. Each entry has only `name = "<label>"` and `path = "builtin:<name>"`, with no hotkey and no gain line. `stop_all_hotkey` stays as it is. The commented file example (with its example hotkey) stays.
- [x] Rewrite the comment above the list. It explains that `builtin:<name>` plays a sound embedded in the app, that the list with credits is in `SOURCES.md` (`crates/app/assets/sounds/` in the source) and in the settings window's "Add built-in…", and that hotkeys are set in the settings window or by adding a `hotkey` line.
- [x] Tests:
  - Replace `default_template_parses_and_references_builtin_demo` with a test that the template's sounds equal the registry minus the demo, as `(label, "builtin:<name>")` pairs in order. Each has no hotkey, and each path resolves to `SoundSource::Builtin`.
  - Update `hotkeys::default_template_resolves_without_warnings`: no warnings, N sounds, all with hotkey `None`, and stop-all is set.
  - Add an `actions.rs` test that the `ActionMap` built from the default template has every built-in in the menu and only the stop-all hotkey.
- [x] Update `form.rs::the_default_template_is_valid` (row count from the registry). Rebase the `save.rs` template tests (round trip, remove-all keeps the list comment, mic toggle kept, refused change, symlink) on the template's first entry (`Air horn`) instead of `DEMO_ENTRY`.
- [x] Run the test suite; it must pass before Task 4.

### Task 4: `play builtin:<name>` and `play --output-only`

**Files:**
- Modify: `crates/app/src/cli.rs`, `crates/app/src/main.rs`

- [x] Change `Command::Play` to `{ file: Option<PathBuf>, output_only: bool }`, where `output_only` is `#[arg(long)]`, documented as "play on the default output only, never into the virtual mic, whatever `[mic] enabled` says". The `file` help mentions `builtin:<name>` (for example `builtin:airhorn`; the names are in `SOURCES.md`, and an unknown one lists them). `main.rs` passes the flag through to `run_play(file, output_only)`.
- [x] `load_play_sound` resolves a UTF-8 argument with `SoundSource::resolve(text, Path::new(""))`, so relative paths stay relative to the working directory. A non-UTF-8 path stays `SoundSource::File`, `None` is the demo entry, and the error context names the argument.
- [x] Add a pure `play_mic_config(mic: MicConfig, output_only: bool) -> MicConfig` that turns `enabled` off when `output_only` is set. `run_play` applies it before logging and `play_to_end`, so neither the mic sink nor the mic wait starts.
- [x] Tests:
  - parsing: `play --output-only`, `play --output-only builtin:airhorn`, `play -- -odd.wav`, and the defaults without the flag;
  - `load_play_sound(Some("builtin:airhorn"))` decodes with label `builtin:airhorn`;
  - an unknown built-in fails, naming the available sounds;
  - `play_mic_config` with `enabled = true` and `output_only` gives `headless_mic_settings(..) == None`;
  - without the flag it keeps the mic.

  The existing demo tests are adapted.
- [x] Run the test suite; it must pass before Task 5.

### Task 5: "Add built-in…" in the settings window

**Files:**
- Modify: `crates/app/src/settings/form.rs`, `crates/app/src/settings/view.rs`, `crates/app/src/settings/save.rs` (tests)

- [ ] Add `SettingsForm::add_builtin(name) -> Result<()>`. It looks the name up through `config::builtin`; an unknown name is an error. It appends `SoundRow { name: label, path: "builtin:<name>", hotkey: None, original: None }`, and does nothing on a form that is not editable (like `add`).
- [ ] In `view.rs`, add `ui.menu_button("Add built-in…", …)` next to "Add sound…", listing `BUILTIN_SOUNDS` by label. Choosing one calls `add_builtin(entry.name)` and closes the menu; an error goes to the message line. The file column is unchanged (it shows the `builtin:` path).
- [ ] Tests:
  - `add_builtin` appends the expected row, refuses an unknown name, and does nothing on a failed form;
  - a `save.rs` test adds a built-in row through the form, saves it with `save_sounds` to a temp config, re-reads it, and checks that `hotkeys::resolve` gives no warnings and resolves the row to `SoundSource::Builtin`.
- [ ] Run the test suite; it must pass before Task 6.

### Task 6: Audition from the settings window

**Files:**
- Create: `crates/app/src/settings/audition.rs`
- Modify: `crates/app/src/settings/mod.rs`, `crates/app/src/settings/view.rs`

- [ ] Add a pure `audition_target(row_path, config_dir) -> Result<OsString, String>`. It uses `SoundSource::resolve`: `Builtin` gives `entry.path` and `File` gives the absolute path. A relative row path is resolved against the config's folder, the way the tray resolves it. A resolve error (empty path, unknown built-in) is shown and nothing is spawned.
- [ ] Add the spawning trait and the controller:
  - The trait is `AuditionSpawner { fn spawn(&mut self, id: u64, target: &OsStr) -> Result<Box<dyn RunningAudition>> }`, where `RunningAudition::kill(&mut self)` kills and reaps the child synchronously.
  - The pure controller is `Auditions<S>`:
    - `play(row, target)` stops the current audition first, then spawns with a new id.
    - `stop()` kills the current one.
    - `playing() -> Option<usize>` is the row whose button reads "Stop".
    - `exited(id, ExitReport)` clears the current audition when the id matches and returns the failure message for a non-zero exit. It ignores a stale id, such as one already killed.
    - `row_removed(index)` stops the audition if that row plays, and shifts the index if a row above it was removed.
    - `close()` kills any running child.
- [ ] Add the real spawner, `ProcessAudition`:
  - The command `fn` is injected, like `ProcessSpawner`. In production it is `current_exe() play --output-only -- <target>`, with stdin and stdout null and stderr piped.
  - The `Child` is shared as `Arc<Mutex<Child>>`, after taking its stderr out.
  - A waiter thread reads stderr to EOF (keeping at most the last 8 KB), then locks and `wait()`s, sends `(id, ExitReport)` on an `mpsc` channel and calls the repaint callback. The UI never polls.
  - `kill` locks, kills (an already-exited child is fine) and waits. Dropping the handle kills, as a backstop.
  - If the waiter thread cannot be started, the child is killed and the spawn fails.
  - A pure `failure_message(stderr, status)` gives the text from the last `Error:` line to the end joined into one line, or the last non-empty line, or the exit status.
- [ ] Wire it into the window:
  - `mod.rs` builds `Auditions<ProcessAudition>` in the eframe creator closure, with `cc.egui_ctx` for the repaint, and passes it and the receiver to `SettingsApp`.
  - Each frame, `view.rs` drains the receiver into `exited` and puts any message on the message line.
  - Each row gets a "Play"/"Stop" button next to its hotkey field, through a new `Edit::Audition(index)`. Play resolves the row's current path and calls `play`; Stop calls `stop`. `Edit::Remove` calls `row_removed`.
  - A spawn or resolve failure goes to the message line.
  - `eframe::App::on_exit` (check the glow-feature signature in eframe 0.37) calls `close()`, so Save, Cancel and the close button all kill a running child.
- [ ] Controller tests with a fake spawner that records spawns and kills:
  - start: the row reads as playing, and the target is passed through;
  - stop: the child is killed;
  - switching rows: the first is killed before the second is spawned;
  - exit observed: playing clears, a zero exit gives no message, a non-zero exit gives one;
  - a stale exit after a kill is ignored;
  - a failed spawn leaves nothing playing and returns the error;
  - `row_removed` stops the playing row or shifts it;
  - `close` kills.
- [ ] Other tests:
  - `audition_target`: built-in, relative and absolute file paths, unknown built-in, and empty path;
  - `failure_message` cases;
  - `ProcessAudition` with the test binary and `--list`: the exit is reported once with success, a missing program fails to spawn and reports nothing, and kill after exit is harmless;
  - the production command's args are `["play", "--output-only", "--", target]`.
- [ ] Run the test suite; it must pass before Task 7.

### Task 7: Verify acceptance criteria

- [ ] Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets` (zero warnings), `cargo test --workspace` and `cargo check --workspace --target x86_64-pc-windows-msvc`.
- [ ] Run the Linux cross-check `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu` with the stub `PKG_CONFIG` and `PKG_CONFIG_ALLOW_CROSS=1`, as CLAUDE.md describes.
- [ ] Re-run `scripts/convert-sounds.sh ~/Downloads/babound-sounds` and confirm that no tracked file changed (`git status --porcelain`).
- [ ] Confirm against the acceptance criteria:
  - every CC0 sound is in the registry, `SOURCES.md` and the template;
  - the budget test passes;
  - the template and hotkey tests show no sound hotkey;
  - the form and audition tests and the `--output-only` tests exist and pass.
- [ ] Check that the new code in `config.rs`, `cli.rs`, `form.rs` and `audition.rs` is covered by tests (aim for 80%+ of the new logic).

### Task 8: Update documentation

- [ ] README:
  - the features list mentions the built-in CC0 set and links to `crates/app/assets/sounds/SOURCES.md`;
  - fix the first-start text (no sound hotkey by default, stop-all stays) and the fallback-config text;
  - in the config example and the path forms: `builtin:<name>`, and the built-ins have no hotkey until the user sets one;
  - in the settings window section: "Add built-in…", and each row's Play/Stop auditions the sound on the default output only, never in the call (one at a time, stopped when the window closes);
  - in the headless playback section: `play builtin:<name>` and `play --output-only`.
- [ ] CLAUDE.md:
  - the `config.rs` bullet describes the registry (`BuiltinSound`, `BUILTIN_SOUNDS`, `builtin()`, `SoundSource::Builtin`, the case-insensitive resolve, the template's built-in list without hotkeys);
  - the assets line documents `scripts/convert-sounds.sh <staging-dir>` and `SOURCES.md` next to `gen_demo_wav`;
  - the `cli.rs` bullet documents `--output-only` and `builtin:` resolution;
  - the `settings/` bullets document `add_builtin`, the "Add built-in…" menu and `audition.rs` (`Auditions`, `AuditionSpawner`, `ProcessAudition`, kill on close);
  - the "No real audio from agents" section notes that agents never click Play and that audition tests use a fake spawner or the test binary with `--list`.

## Post-Completion

- Install vorbis-tools (`brew install vorbis-tools`) before running the plan, if it is still missing.
- Human check, not automated: open the settings window, audition a few built-ins and a file row, and confirm the sound plays on the default output only, with the mic on as well, and that Stop and closing the window cut it off.
