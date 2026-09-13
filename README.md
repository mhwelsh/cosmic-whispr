# cosmic-whispr

A COSMIC panel applet for dictation. Press a shortcut, speak, press it again:
the transcript is typed into the window you are already working in, or copied
to the clipboard.

Transcription goes to any OpenAI-compatible endpoint: OpenAI, Groq, or a local
whisper.cpp or faster-whisper server.

## Install

Arch and derivatives:

```sh
paru -S cosmic-whispr            # once published to the AUR
cd packaging/aur && makepkg -si  # or build the PKGBUILD directly
```

From source, with a Rust toolchain and ALSA headers:

```sh
git clone https://github.com/mhwelsh/cosmic-whispr
cd cosmic-whispr
just install
```

Then add **Whispr Dictation** in *Settings → Desktop → Panel → Applets*.

## Set the API key

The key is kept in the system keyring. Paste it:

```sh
cosmic-whispr --set-key      # reads stdin, so it stays out of shell history
```

or import it from 1Password:

```sh
cosmic-whispr --set-key-from op://Private/openai-api/credential
```

Either also works from the applet popup. A local server on `localhost` needs
no key at all; the key is only ever sent over HTTPS or to a loopback address.

## Use

Nothing is bound by default. Add custom shortcuts in *Settings → Desktop →
Keyboard Shortcuts* for whichever of these you want, on whatever keys are
free for you:

| Command | Effect |
| --- | --- |
| `cosmic-whispr --toggle` | Type into the focused window |
| `cosmic-whispr --toggle --clipboard` | Copy it instead |

Binding both gives you a key for each destination. Whichever one starts the
recording decides where that transcript goes. `--clipboard` needs
`wl-clipboard` installed.

Left-click the panel icon to start and stop, right-click for settings and the
last transcript. A shortcut is better than clicking, which moves keyboard
focus to the panel.

| Command | Effect |
| --- | --- |
| `--start` / `--stop` / `--cancel` | Push-to-talk control |
| `--check` | Report endpoint, key, microphone, typing and clipboard support |
| `--type-test` | Type a known phrase, to test keystrokes alone |
| `--list-devices` | Input devices, for the microphone setting |
| `--clear-key` | Forget the saved key |

## Settings

In the popup: endpoint, model, language, and a biasing prompt for names and
jargon; microphone; keystroke delay, if an application drops fast keystrokes;
a trailing space after each typed transcript; and an optional second pass that
strips filler words with a small chat model.

## Troubleshooting

Start with `cosmic-whispr --check`, which reports each prerequisite separately.

- **Nothing is typed.** `--type-test` isolates keystroke delivery from the
  microphone and the network. If characters are dropped, raise the keystroke
  delay.
- **`in use: NOT SET`.** No key stored. Run `--set-key` or `--set-key-from`.
- **`Keyring unavailable`.** Nothing is serving `org.freedesktop.secrets`.
  Check that `gnome-keyring-daemon` is running with its `secrets` component.
- **`the API key is withheld from this endpoint`.** The endpoint is plain HTTP
  and not loopback.
- **`no audio captured`.** The microphone is muted, or the wrong device is
  selected.

For logs, run from a terminal with `COSMIC_WHISPR_LOG=debug`.

## Build

```sh
just build-release
just test
just check        # clippy
just validate     # desktop entry and AppStream metadata
```

Packaging lives in `packaging/aur/`.

## License

MPL-2.0. See [LICENSE](LICENSE).
