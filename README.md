# Sound-Babound

A cross-platform tray soundboard written in Rust. A global hotkey plays a preloaded sound through the system default output device.

Playback keeps going when the default output changes. Switch it in the OS settings, or unplug and replug a headset, and the sounds that are playing move to the new default device and continue from where they were. Plugging in a device that does not become the default causes no audible gap. On Linux the app plays through ALSA's `default` device and relies on PipeWire or PulseAudio to move the stream when the default output changes. On bare ALSA a default switch is not followed; only losing the device triggers a rebuild.

Features:

- a tray icon with a menu listing every configured sound, plus "Stop all", "Reload config", "Open config folder" and "Quit"
- one global hotkey per sound and an optional stop-all hotkey
- overlapping playback (up to 32 voices; the oldest voice is evicted beyond that)
- WAV, FLAC, OGG Vorbis and MP3, at any sample rate (resampled to the device rate), mono or stereo (files with more channels play their first two)
- per-sound gain and a master gain
- a headless `play` command for one-off playback from a terminal

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

With no arguments the app starts in the tray. There is no window. On macOS there is no Dock icon either. On first start it writes a commented default config (see below) that binds the embedded demo chime to `CmdOrCtrl+Shift+1` and stop-all to `CmdOrCtrl+Shift+0`.

On macOS the hotkeys use the Carbon hotkey API, so no Accessibility permission is needed.

### Headless playback

```sh
sound-babound play               # plays the embedded demo chime
sound-babound play path/to/file.mp3
```

`play` decodes the file first, so a bad file fails right away with an error that names it. It then plays the sound through the default output device and exits when the sound has finished. Ctrl+C stops it. If no output stream comes up within 10 seconds, it exits with an error. If the sound is still playing 10 seconds after its own length (for example because the output device went away and no other device took over), it exits with an error too. Any failure prints a readable message and exits with code 1.

On Windows the binary uses the GUI subsystem, so it attaches to the console it was started from whenever arguments are given. That way `play`, `--help` and `--version` output shows up in the terminal. Because it is a GUI-subsystem binary, `cmd.exe` returns to the prompt without waiting for it. Use `start /wait sound-babound play ...` to wait for `play` to finish and read its exit code from `%ERRORLEVEL%`.

### Logging

Logs go to stderr through `tracing`. The default level is `info`. Use `RUST_LOG` to change it:

```sh
RUST_LOG=debug sound-babound play
RUST_LOG=babound_engine=debug,info sound-babound
```

On Windows, tray mode (no arguments) has no console, so its logs and startup errors are not visible anywhere.

At `info` you can see every output-stream rebuild with its device name, sample rate, channel count and sample format. At `debug` you also see device events that were checked and dropped because the default output did not change.

## Configuration

The config file is `config.toml` in a `sound-babound` folder under the OS config directory:

| OS      | Path                                                         |
|---------|--------------------------------------------------------------|
| macOS   | `~/Library/Application Support/sound-babound/config.toml`    |
| Windows | `%APPDATA%\sound-babound\config.toml`                        |
| Linux   | `$XDG_CONFIG_HOME/sound-babound/config.toml` (default `~/.config/sound-babound/config.toml`) |

The file is created with defaults on first start and is never overwritten after that. "Open config folder" in the tray menu opens its folder. After editing, choose "Reload config". New sounds, hotkeys and gains apply right away, and sounds that are already playing keep playing. If the new file does not parse, the error is logged and the old config stays in effect.

At startup, a config that does not parse is fatal: the app logs the error and exits with code 1. If the file cannot be read at all, the built-in default config (the demo chime) is used and a warning is logged.

Example:

```toml
# Overall volume applied to every sound, 0.0 (silent) to 4.0. Default 1.0.
master_gain = 1.0

# Hotkey that stops every sound that is playing. Optional.
stop_all_hotkey = "CmdOrCtrl+Shift+0"

[[sound]]
name = "Demo chime"          # label in the tray menu (required)
path = "builtin:demo"        # the embedded chime
hotkey = "CmdOrCtrl+Shift+1" # optional
gain = 1.0                   # optional, 0.0 to 4.0, default 1.0

[[sound]]
name = "Airhorn"
path = "sounds/airhorn.mp3"  # relative to the config folder
hotkey = "CmdOrCtrl+Shift+2"
gain = 0.8

[[sound]]
name = "Rimshot"
path = "/Users/me/Music/rimshot.wav"  # absolute paths are used as-is
```

Path forms:

- `builtin:demo` is the chime embedded in the binary.
- Absolute paths are used as they are.
- Relative paths are resolved against the folder that contains `config.toml`.

All sounds are decoded into memory when the config is loaded. A file that cannot be decoded is logged and left out of the menu. Gains outside [0, 4] are clamped, with a warning in the log.

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

A hotkey that does not parse, or that is already used by an earlier entry, is skipped with a warning in the log. The sound stays playable from the tray menu. If another application already holds a hotkey, registering it fails. That failure is logged too, and the rest still work.

## Wayland limitation

Global hotkeys on Linux are X11 key grabs. Wayland compositors do not deliver those to other applications. In a Wayland session (detected from `XDG_SESSION_TYPE`, or from a non-empty `WAYLAND_DISPLAY` when `XDG_SESSION_TYPE` is unset or not `wayland`, `x11` or `tty`), the app does not register hotkeys and logs a warning. The tray menu still works, so sounds can be played from it. To use hotkeys, log in to an X11 session.

## Project layout

- `crates/engine` (`babound-engine`): decoding, the realtime mixer, the cpal output sink, the device supervisor and the platform device watchers. It has no UI dependencies.
- `crates/app` (`sound-babound`): the binary, with the tray, global hotkeys, the TOML config and the `play` CLI.

## License

MIT, see [LICENSE](LICENSE).
