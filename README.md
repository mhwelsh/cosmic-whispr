# cosmic-whispr

A COSMIC panel applet for dictation. Press a shortcut to start recording,
speak, then press it again to stop. The transcript is typed into the focused
window or copied to the clipboard.

Transcription uses any OpenAI-compatible endpoint: OpenAI, Groq, or a local
whisper.cpp or faster-whisper server.

## Install

On Arch and derivatives, use the bundled PKGBUILD:

```sh
git clone https://github.com/mhwelsh/cosmic-whispr
cd cosmic-whispr/packaging/aur && makepkg -si
```

Elsewhere, build from source. This requires a Rust toolchain and ALSA headers:

```sh
git clone https://github.com/mhwelsh/cosmic-whispr
cd cosmic-whispr
just install
```

Then add **Whispr Dictation** in *Settings → Desktop → Panel → Applets*.

## Set the API key

The key is stored in the system keyring. To paste it:

```sh
cosmic-whispr --set-key      # reads stdin, keeping the key out of shell history
```

To import it from 1Password:

```sh
cosmic-whispr --set-key-from op://Private/openai-api/credential
```

Both commands are also available in the applet popup. A local server on
`localhost` requires no key. The key is sent only over HTTPS or to a loopback
address.

## Use

No shortcuts are bound by default. Add them in *Settings → Desktop → Keyboard
Shortcuts*:

| Command | Effect |
| --- | --- |
| `cosmic-whispr --toggle` | Type into the focused window |
| `cosmic-whispr --toggle --clipboard` | Copy to the clipboard |

Bind both to get a key for each destination. The command that starts the
recording determines where that transcript goes. `--clipboard` requires
`wl-clipboard`.

Left-click the panel icon to start and stop recording. Right-click for
settings and the last transcript. Clicking moves keyboard focus to the panel,
so a shortcut is preferable.

Additional commands:

| Command | Effect |
| --- | --- |
| `--start` / `--stop` / `--cancel` | Push-to-talk control |
| `--check` | Report endpoint, key, microphone, typing and clipboard support |
| `--type-test` | Type a known phrase to test keystroke delivery |
| `--list-devices` | List input devices for the microphone setting |
| `--clear-key` | Delete the saved key |

## Settings

The applet popup configures:

- Endpoint, model, language, and a biasing prompt for names and jargon
- Microphone
- Keystroke delay, for applications that drop fast keystrokes
- A trailing space after each typed transcript
- An optional second pass that strips filler words using a small chat model

## Troubleshooting

Run `cosmic-whispr --check` first.

- **Nothing is typed.** Run `--type-test` to isolate keystroke delivery from
  the microphone and the network. If characters are dropped, increase the
  keystroke delay.
- **`in use: NOT SET`.** No key is stored. Run `--set-key` or `--set-key-from`.
- **`Keyring unavailable`.** No service is providing `org.freedesktop.secrets`.
  Check that `gnome-keyring-daemon` is running with its `secrets` component.
- **`the API key is withheld from this endpoint`.** The endpoint uses plain
  HTTP and is not loopback.
- **`no audio captured`.** The microphone is muted, or the wrong device is
  selected.

To collect logs, set `COSMIC_WHISPR_LOG=debug` and run from a terminal.

## Build

```sh
just build-release
just test
just check        # clippy
just validate     # desktop entry and AppStream metadata
```

Packaging files are in `packaging/aur/`.

## License

MPL-2.0. See [LICENSE](LICENSE).
