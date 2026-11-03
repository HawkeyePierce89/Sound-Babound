# Sound-Babound

A cross-platform tray soundboard written in Rust. A global hotkey plays a preloaded sound through the system default output device.

Playback keeps going when the default output changes. Switch it in the OS settings, or unplug and replug a headset, and the sounds that are playing move to the new default device and continue from where they were. Plugging in a device that does not become the default causes no audible gap. On Linux the app plays through ALSA's `default` device and relies on PipeWire or PulseAudio to move the stream when the default output changes. On bare ALSA a default switch is not followed; only losing the device triggers a rebuild.

Features:

- a tray icon with a menu listing every configured sound, plus the virtual mic status, "Mic to call", "Stop all", "Settings…", "Reload config", "Open config folder" and "Quit"
- one global hotkey per sound and an optional stop-all hotkey
- overlapping playback (up to 32 voices; the oldest voice is evicted beyond that)
- WAV, FLAC, OGG Vorbis and MP3, at any sample rate (resampled to the device rate), mono or stereo (files with more channels play their first two)
- a built-in set of twelve CC0 sounds (air horn, rimshot, sad trombone and so on) embedded in the app, with credits in [`crates/app/assets/sounds/SOURCES.md`](crates/app/assets/sounds/SOURCES.md)
- per-sound gain and a master gain
- a headless `play` command for one-off playback from a terminal
- an optional virtual microphone ("Mic to call"): the sounds, mixed with your real microphone, go into a voice or video call through a virtual audio cable (see [Virtual microphone](#virtual-microphone))
- a `mic-check` command that reports whether the virtual microphone can work
- a settings window to add sounds (files or built-ins), remove and rename them, set their hotkeys and audition them on the default output, with the virtual microphone status (see [Settings window](#settings-window))

Supported platforms: macOS, Windows 10/11 (x64 and arm64), and Linux (X11; see the Wayland limitation below). So far only macOS has been run on real hardware. Windows x64 and Linux x64 are compile-checked; Windows arm64 and Linux aarch64 builds have not been tried yet.

## Building

You need Rust 1.98 or newer.

```sh
cargo build --release
```

The binary is `target/release/sound-babound` (`sound-babound.exe` on Windows).

### Linux build dependencies

The audio backend needs ALSA headers. The tray and the event loop need GTK 3 and an AppIndicator library. On Debian or Ubuntu:

```sh
sudo apt install pkg-config libasound2-dev libgtk-3-dev libayatana-appindicator3-dev
# or libappindicator3-dev instead of libayatana-appindicator3-dev
```

On Arch:

```sh
sudo pacman -S pkgconf alsa-lib gtk3 libayatana-appindicator  # or libappindicator-gtk3
```

`libxdo` is not needed: the tray uses the default `tray-icon` features, which do not link it. Global hotkeys use X11 through the pure-Rust `x11rb`, so they need no extra headers.

macOS and Windows need no extra system packages.

## Running

### Tray mode

```sh
sound-babound
```

With no arguments the app starts in the tray. There is no window. On macOS there is no Dock icon either. On first start it writes a commented default config (see below) that lists the twelve built-in sounds, with no hotkey on any of them, and binds stop-all to `CmdOrCtrl+Shift+0`. Set a sound's hotkey in the settings window or in the config file.

On macOS the hotkeys use the Carbon hotkey API, so no Accessibility permission is needed. To use the virtual microphone on macOS, run the app bundle instead (see [macOS](#macos)).

The tray menu has, from top to bottom:

- one item per configured sound, which plays it;
- the virtual mic status line (greyed out, for reading only, see [Tray status](#tray-status));
- "Mic to call: on" or "Mic to call: off", which turns the virtual microphone on or off and saves the choice to the config file;
- "Stop all", "Settings…" (see [Settings window](#settings-window)), "Reload config", "Open config folder" and "Quit".

### Settings window

```sh
sound-babound settings
```

The tray's "Settings…" item opens a small settings window; `sound-babound settings` opens the same window from a terminal. Like the tray, it writes the default config file if there is none. The tray opens one window at a time: choosing "Settings…" again while it is open does nothing. On Linux the "Add sound…" file picker goes through the XDG desktop portal, so `xdg-desktop-portal` and a backend for your desktop (`-gtk`, `-gnome` or `-kde`) must be running, as they usually are; the window draws with OpenGL. Neither is needed to build.

The window has two parts:

- "Virtual mic": the same report as `mic-check` (see [Checking the virtual microphone](#checking-the-virtual-microphone)), refreshed about once a second. The window cannot see the tray's running microphone capture: when the default input is the virtual device, it says so and notes that the tray's capture stays on the last real microphone it used, if there was one.
- "Sounds": every configured sound with its name, file and hotkey. You can rename a sound, remove it, add one of the built-in sounds with "Add built-in…" (a menu of the embedded set; the file column shows `builtin:<name>`), add sounds with "Add sound…" (a file picker for WAV, FLAC, OGG and MP3 files; the name starts as the file name without its extension, and the path is stored as an absolute path), and set a hotkey: click the hotkey field, then press the combination. The field records the key the way the tray will match it (see [Hotkey syntax](#hotkey-syntax)): on macOS where the key sits, named as on a US layout (on AZERTY, the key labelled A shows as `Q`); on Windows and Linux the key your layout types (the key labelled A shows as `A`; a symbol the config cannot name, such as `&` on AZERTY's digit row, is recorded by its position, `1`). Escape clears the hotkey. A key without a modifier, or with Shift alone, is accepted only for F1–F24, so a plain or capital letter never becomes a global hotkey. The copy, cut and paste shortcuts cannot be set either. The "Stop all" hotkey is shown, but it is set in the config file.

Each row has a Play button next to its hotkey field, which turns into Stop while the sound plays. It auditions the sound on the default output only, never in the call, whatever `[mic] enabled` says. It runs `sound-babound play --output-only` in a child process, one at a time: starting another row stops the first, and closing the window (Save, Cancel or the close button) stops it too. A sound that cannot be played shows its error in the window.

Problems are shown next to their sound: a blank name, a hotkey from the file that does not parse, or a hotkey used twice (by two sounds, or by a sound and "Stop all"). Save stays disabled while there is any problem. If the config file does not parse, the window shows the error and cannot edit the sounds; fix the file first.

While a window opened from the tray's "Settings…" item is open, the tray's global hotkeys are switched off, so you can assign a combination the tray uses. Save writes the sounds to the config file and closes the window. It changes only each sound's `name`, `path` and `hotkey`: per-sound `gain`, `[mic]`, `master_gain`, `stop_all_hotkey` and every comment are kept, and the file is replaced in one step, so it is never left half written. If the sounds in the file were changed by something else while the window was open, Save refuses and says so, and the file is left as it was. Cancel, or closing the window, writes nothing. When that window closes, the tray reloads the config if the file changed, and turns its hotkeys back on. After saving from a window started with `sound-babound settings`, choose "Reload config" in the tray. Quitting the tray leaves an open settings window running.

### Headless playback

```sh
sound-babound play               # plays the embedded demo chime
sound-babound play path/to/file.mp3
sound-babound play builtin:airhorn   # a built-in sound
sound-babound play --output-only builtin:airhorn   # never into the virtual mic
```

`builtin:<name>` is resolved the same way as in the config file, ignoring case; the names are listed in [`SOURCES.md`](crates/app/assets/sounds/SOURCES.md), plus `builtin:demo`, the chime `play` plays without an argument, and an unknown name fails with the list of available ones. `play` decodes the file first, so a bad file fails right away with an error that names it. It then plays the sound through the default output device and exits when the sound has finished. Ctrl+C stops it. If no output stream comes up within 10 seconds, it exits with an error. If the sound is still playing 10 seconds after its own length (for example because the output device went away and no other device took over), it exits with an error too. Any failure prints a readable message and exits with code 1.

With `[mic] enabled = true` in the config file, `play` also plays the sound into the virtual microphone. It waits up to 3 seconds for the virtual device; if that does not come up, it warns and plays on the default output only. `play` never opens your real microphone, and on Linux it does not create the virtual device (only the tray does). It reads the config file but never creates it; a missing or malformed file means the defaults (mic off), with a note. With `--output-only`, `play` plays on the default output only and never into the virtual microphone, even when `[mic] enabled = true`; it does not wait for the virtual device either.

### Checking the virtual microphone

```sh
sound-babound mic-check
```

`mic-check` reports whether the virtual microphone can work with the `[mic]` section of the config file. It opens no audio stream and plays nothing. It reports:

- the config file it read (a missing or malformed file means the defaults, with a note; it never creates the file);
- whether "Mic to call" is on;
- the virtual device pattern, and the output device it matches with its sample rate, channel count and sample format;
- the input the microphone passthrough would use and its format, including when the default input is the virtual device itself and the app would refuse it;
- the microphone permission (on macOS). This is the permission of the terminal you run `mic-check` from, not of `Sound-Babound.app`; the tray status line shows the app's own. Before macOS 14 the permission cannot be read and is reported as unknown;
- a final verdict, "available", "sounds only" (the call would hear the sounds but not your voice) or "unavailable", with the reasons and what to do about them.

It exits with code 0 when the virtual device is found and the microphone can be captured, and with a non-zero code otherwise. It does not check whether "Mic to call" is on: a setup that would work while it is off still exits with 0.

On Windows the binary uses the GUI subsystem, so it attaches to the console it was started from whenever arguments are given. That way `play`, `--help` and `--version` output shows up in the terminal. Because it is a GUI-subsystem binary, `cmd.exe` returns to the prompt without waiting for it. Use `start /wait sound-babound play ...` to wait for `play` to finish and read its exit code from `%ERRORLEVEL%`.

### Logging

Logs go to stderr through `tracing`. The default level is `info`. Use `RUST_LOG` to change it:

```sh
RUST_LOG=debug sound-babound play
RUST_LOG=babound_engine=debug,info sound-babound
```

On Windows, tray mode (no arguments) has no console, so its logs and startup errors are not visible anywhere. On macOS the app bundle started from Finder has no visible log either. To read it and still keep the bundle's microphone permission, start the bundle with `open --stderr /tmp/sound-babound.log --env RUST_LOG=info target/release/Sound-Babound.app` and read that file.

At `info` you can see every stream rebuild with its device name, sample rate, channel count and sample format: the default output, and with the virtual microphone on, the virtual device and the microphone input too. At `debug` you also see device events that were checked and dropped because the stream's device did not change, and the "device changed" stream errors that some backends report when you switch the default device, which the app expects and handles by rebuilding. A device that keeps failing to open is warned about once, and again only when the error changes; the retries are logged at `debug`.

## Configuration

The config file is `config.toml` in a `sound-babound` folder under the OS config directory:

| OS      | Path                                                         |
|---------|--------------------------------------------------------------|
| macOS   | `~/Library/Application Support/sound-babound/config.toml`    |
| Windows | `%APPDATA%\sound-babound\config.toml`                        |
| Linux   | `$XDG_CONFIG_HOME/sound-babound/config.toml` (default `~/.config/sound-babound/config.toml`) |

The file is created with defaults on first start. After that the app changes it in two cases only: the "Mic to call" tray item rewrites the `enabled` value of `[mic]`, and Save in the [settings window](#settings-window) rewrites the `[[sound]]` entries' names, paths and hotkeys. Both keep your comments and other keys. To keep the two from overwriting each other's change, the app also creates a hidden `.config.toml.lock` file next to the config (the edit lock) and never removes it; leave it in place. If you have unsaved edits to the file, save them before using either. "Open config folder" in the tray menu opens its folder. After editing, choose "Reload config". New sounds, hotkeys and gains apply right away, and sounds that are already playing keep playing. If the new file does not parse, the error is logged and the old config stays in effect.

At startup, the tray always comes up. If the config file does not parse, the app logs the parse error at error level, with the file's path, and starts with the built-in default config (the built-in sound set). The tray menu still works: fix the file, then choose "Reload config" to load it. If the file cannot be read at all, the built-in default config is used too, with a warning in the log. The log goes to stderr (see [Logging](#logging)), so on macOS and Linux start the app from a terminal to read it (for the macOS app bundle, see [Logging](#logging)). On Windows tray mode has no console, so the log is not visible; if the built-in sounds show up instead of your own, check the file for TOML syntax errors.

Example:

```toml
# Overall volume applied to every sound, 0.0 (silent) to 4.0. Default 1.0.
master_gain = 1.0

# Hotkey that stops every sound that is playing. Optional.
stop_all_hotkey = "CmdOrCtrl+Shift+0"

[[sound]]
name = "Air horn"            # label in the tray menu (required)
path = "builtin:airhorn"     # a sound embedded in the app
hotkey = "CmdOrCtrl+Shift+1" # optional; the built-ins have none until you set one
gain = 1.0                   # optional, 0.0 to 4.0, default 1.0

[[sound]]
name = "My airhorn"
path = "sounds/airhorn.mp3"  # relative to the config folder
hotkey = "CmdOrCtrl+Shift+2"
gain = 0.8

[[sound]]
name = "Rimshot"
path = "/Users/me/Music/rimshot.wav"  # absolute paths are used as-is
```

The `[mic]` section configures the virtual microphone (see [Virtual microphone](#virtual-microphone)). Every key is optional, and with no `[mic]` section the virtual microphone is off:

```toml
[mic]
enabled = false                # true sends the sounds and your microphone into the call
virtual_device = "BlackHole"   # the virtual cable's playback device (see below)
sound_gain = 1.0               # volume of the sounds in the call, 0.0 to 4.0
mic_gain = 1.0                 # volume of your microphone in the call, 0.0 to 4.0
```

| Key              | Default | Meaning |
|------------------|---------|---------|
| `enabled`        | `false` | Turns the virtual microphone on. The tray's "Mic to call" item flips this value in the file, keeping your comments and other keys. |
| `virtual_device` | `"BlackHole"` on macOS, `"CABLE Input"` on Windows, `"sound_babound_sink"` on Linux | The output device the call mix is written to: the first output whose name or id contains this text, ignoring case. A missing or blank value means the default. On macOS the default prefers "BlackHole 2ch" when several BlackHole devices are installed (for example 2ch and 16ch), and takes any BlackHole device otherwise; a pattern you set yourself has no preference, so the first matching output is used. On Linux the app creates its own devices whatever this says (see [Linux](#linux)). |
| `sound_gain`     | `1.0`   | Volume of the sounds in the call, on top of their own gain and `master_gain`. It does not change what you hear. |
| `mic_gain`       | `1.0`   | Volume of your microphone in the call. |

Gains outside [0, 4] are clamped, with a warning in the log. "Reload config" applies a changed `[mic]` section without interrupting the sounds you hear; only the virtual microphone is restarted, and a change of the gains alone restarts nothing.

Path forms:

- `builtin:<name>` is a sound embedded in the binary, for example `builtin:airhorn` (names are case-insensitive). The twelve sounds of the default config are listed with their credits in [`SOURCES.md`](crates/app/assets/sounds/SOURCES.md), and `builtin:demo` is a short chime. Built-in sounds have no hotkey until you set one.
- Absolute paths are used as they are.
- Relative paths are resolved against the folder that contains `config.toml`.

All sounds are decoded into memory when the config is loaded. A file that cannot be decoded is logged and left out of the menu. Gains outside [0, 4] are clamped, with a warning in the log.

## Virtual microphone

With "Mic to call" on, the app writes a second mix into a virtual audio cable: the sounds you play plus your real microphone. In the call app (Zoom, Teams, Discord, Google Meet and so on) you choose the cable as your microphone, so the other participants hear both your voice and the sounds. You keep hearing the sounds on your default output as before, but not your own voice.

The app follows your system default input as the microphone and keeps up with it when it changes, just like the default output. If the default input is the virtual device itself, the app refuses to capture it, since that would feed the call mix back into itself. It falls back to the last real microphone that was the default input while it ran, if that is still connected, and logs a warning. Otherwise the call hears the sounds only until you choose a real microphone as the default input. On Linux the app checks the default input and the list of inputs once a second, so a switch can take up to about a second to be picked up, and the fallback microphone being unplugged is noticed within about a second too.

The virtual microphone is off by default, and while it is off the app opens no extra audio device. Turn it on with the "Mic to call" tray item or `[mic] enabled = true`, then run `sound-babound mic-check` to see whether everything is in place.

### macOS

1. Install [BlackHole](https://github.com/ExistentialAudio/BlackHole), the 2-channel version, for example with `brew install blackhole-2ch`. BlackHole is a separate project under the GPL-3.0 licence; Sound-Babound does not bundle it, it only writes to it as an output device.
2. Build the app bundle:

   ```sh
   scripts/bundle-macos.sh
   ```

   This builds the release binary and assembles `target/release/Sound-Babound.app`, signed ad hoc for this machine (it is not notarized, so it is meant to run where it was built). Start it from Finder or with `open target/release/Sound-Babound.app`. macOS only asks for, and remembers, the microphone permission for an app bundle. The bare binary started from a terminal gets the terminal's permission instead.
3. Turn on "Mic to call". The first time the app opens the microphone, macOS asks for permission. Allow it.
4. In the call app, choose "BlackHole 2ch" as the microphone. Keep the call app's speaker on your headphones or speakers, not on BlackHole.

If you denied the permission, the call hears the sounds but not your voice, and the tray status says "microphone access denied". To allow it later, open System Settings > Privacy & Security > Microphone and turn on Sound-Babound. While "Mic to call" is on, the app checks the permission about once a second and opens the microphone as soon as it is allowed; if the call still does not hear you, quit and restart the app. If Sound-Babound is not in that list, `tccutil reset Microphone io.github.hawkeyepierce89.sound-babound` makes macOS ask again on the next start. Each rebuild of the bundle changes its ad hoc signature, so macOS may ask again after a rebuild; if it refuses without asking, use the same `tccutil reset` command.

Reading the permission needs macOS 14 or later. On older macOS the app always tries to open the microphone. If access was denied, the call hears silence instead of your voice, and the tray does not say so: check System Settings > Privacy & Security > Microphone.

If you use another virtual cable (Loopback, for example), set `virtual_device` to the name of its output device.

The app asks BlackHole and your microphone for 10 ms buffers, which keeps the delay your voice gets in the call short. If a device keeps a larger buffer (a DAW or Audio MIDI Setup may have set BlackHole to 2048 frames or more), the microphone still reaches the call: the app holds enough audio to cover both buffers, so the call hears you with a higher delay.

### Windows

1. Install [VB-CABLE](https://vb-audio.com/Cable/) (a separate, donationware driver) and restart if the installer asks.
2. Turn on "Mic to call". The app writes to "CABLE Input".
3. In the call app, choose "CABLE Output" as the microphone.

Keep your real microphone as the Windows default input. If "CABLE Output" becomes the default input, the app refuses to capture it, as described above. With another cable, set `virtual_device` to its playback side ("CABLE-A Input", "VoiceMeeter Input"); the app then also refuses its recording side, the same name with "Input" changed to "Output".

### Linux

The app creates the virtual microphone itself while "Mic to call" is on: a "Sound-Babound Mic" input (`sound_babound_mic`), fed by an output named "Sound-Babound Sink" (`sound_babound_sink`) that the app writes the call mix to. Do not choose either one as your default device. The app removes both when you turn "Mic to call" off or quit, and removes any that a crashed run left behind before it creates them again. If the sound server restarts while "Mic to call" is on, the app creates them again within a few seconds. It creates them whenever "Mic to call" is on, even if `virtual_device` names another output. There is nothing to install apart from `pactl`, which it uses to create the devices: install `pulseaudio-utils` on Debian or Ubuntu, or `libpulse` on Arch. This works with both PulseAudio and PipeWire (through `pipewire-pulse`). On Linux the virtual microphone and the microphone passthrough use the PulseAudio API; the sounds you hear still play through ALSA's `default` device. The `pactl` calls run in the background, so the tray stays responsive while the devices are created or removed; the status line shows "starting" meanwhile. If the sound server stops answering, turning "Mic to call" off can take a few seconds and quitting up to about 13 seconds; the log then warns that a device query, a `pactl` call or the virtual mic worker did not finish, and the app goes on without waiting for it; the virtual devices may then stay until the next start removes them.

In the call app, choose "Sound-Babound Mic" as the microphone. `mic-check` does not create the device, so it reports it as not found unless the tray app is running with "Mic to call" on.

### Tray status

The greyed-out status line in the tray menu shows the state of the virtual microphone. It updates about once a second while "Mic to call" is on.

| Status | Meaning |
|--------|---------|
| Virtual mic: off | "Mic to call" is off. |
| Virtual mic: starting | The virtual device is being opened (on Linux, also while the app creates it, or while it waits for the previous mic stream to stop). |
| Virtual mic: BlackHole 2ch (active) | The call mix is going to that device. |
| Virtual mic: … (active, microphone access denied) | The call hears the sounds only. On macOS, allow the microphone as described above. |
| Virtual mic: … (active, microphone unavailable: *reason*) | The microphone cannot be opened, and the call hears the sounds only. The reason is "default input is the virtual mic device" (choose a real microphone as the default input), "no input device", "input device not found", or "error, see log" (the log has the full error). |
| Virtual mic: not found, install BlackHole / install VB-CABLE | No output device matches `virtual_device`. The app keeps looking, so installing or plugging in the device is enough. |
| Virtual mic: could not create it, see log | Linux only: `pactl` failed or is missing, or no output matches a custom `virtual_device`. |
| Virtual mic: error, see log | The virtual device was found but could not be opened. The app keeps retrying. |

## Hotkey syntax

A hotkey is any number of modifiers followed by exactly one key, joined with `+`. The key must come last: `Shift+Ctrl+1` works, `1+Ctrl` does not. Examples: `CmdOrCtrl+Shift+1`, `Ctrl+Alt+F5`, `Cmd+Shift+A` or `Alt+Space`. Names are case-insensitive, and spaces around `+` are ignored.

Modifiers:

| Modifier     | Also accepted                 | Meaning                                    |
|--------------|-------------------------------|--------------------------------------------|
| `Ctrl`       | `Control`                     | Control                                    |
| `Alt`        | `Option`, `Opt`               | Alt / Option                               |
| `Shift`      |                               | Shift                                      |
| `Cmd`        | `Command`, `Super`, `Meta`, `Win`, `Windows` | Command on macOS, the Windows/Super key elsewhere |
| `CmdOrCtrl`  | `CommandOrControl`, `CmdOrControl`, `CommandOrCtrl` | Cmd on macOS, Ctrl on Windows and Linux |

Keys:

- letters `A`–`Z` and digits `0`–`9`
- function keys `F1`–`F24`
- `Space`, `Enter` (or `Return`), `Tab`, `Escape`, `Backspace`, `Delete`, `Insert`, `Home`, `End`, `PageUp`, `PageDown`
- arrows `Up`, `Down`, `Left`, `Right`
- numpad `Num0`–`Num9`
- punctuation: `Minus`, `Equal`, `Comma`, `Period`, `Slash`, `Semicolon`, `Quote`, `Backquote`, `BracketLeft`, `BracketRight`, `Backslash`

On macOS key names are key positions, named as on a US layout: on another layout a name means the key in that place (on AZERTY, `Q` is the key labelled A). On Windows and Linux letter and symbol names follow the keyboard layout: `A` is the key that types A (on AZERTY, the key labelled A), and digits are the digit row's keys.

A hotkey that does not parse, or that is already used by an earlier entry, is skipped with a warning in the log. The sound stays playable from the tray menu. If another application already holds a hotkey, registering it fails. That failure is logged too, and the rest still work.

## Wayland limitation

Global hotkeys on Linux are X11 key grabs. Wayland compositors do not deliver those to other applications. In a Wayland session (detected from `XDG_SESSION_TYPE`, or from a non-empty `WAYLAND_DISPLAY` when `XDG_SESSION_TYPE` is unset or not `wayland`, `x11` or `tty`), the app does not register hotkeys and logs a warning. The tray menu still works, so sounds can be played from it. To use hotkeys, log in to an X11 session.

## Project layout

- `crates/engine` (`babound-engine`): decoding, the realtime mixers, the cpal output and capture streams, the microphone passthrough, the device supervisors and the platform device watchers. It has no UI dependencies.
- `crates/app` (`sound-babound`): the binary, with the tray, global hotkeys, the TOML config, the settings window, the `play`, `mic-check` and `settings` CLI, and the per-OS glue (the Linux virtual device, the macOS microphone permission).
- `crates/app/macos/Info.plist` and `scripts/bundle-macos.sh`: the macOS app bundle.
- `crates/app/assets/sounds/` and `scripts/convert-sounds.sh`: the built-in sounds (OGG Vorbis) and their credits in `SOURCES.md`, both generated by `scripts/convert-sounds.sh <staging-dir>` (needs `ffmpeg`, `ffprobe` and `oggenc`). Do not edit `SOURCES.md` by hand.

## License

MIT, see [LICENSE](LICENSE). The built-in sounds in `crates/app/assets/sounds/` are third-party recordings released under CC0 1.0; see [`SOURCES.md`](crates/app/assets/sounds/SOURCES.md) for their sources.
