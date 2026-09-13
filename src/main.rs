// SPDX-License-Identifier: MPL-2.0

//! cosmic-whispr — dictation for the COSMIC panel.
//!
//! Records from the microphone, reduces the audio to 16 kHz mono PCM, sends
//! it to an OpenAI-compatible transcription endpoint, and types the result
//! into whatever window has keyboard focus.

mod app;
mod audio;
mod clipboard;
mod cleanup;
mod config;
mod ipc;
mod secret;
mod stt;
mod typer;

use tracing_subscriber::EnvFilter;

const HELP: &str = "\
cosmic-whispr — dictation applet for the COSMIC panel

USAGE:
    cosmic-whispr                 Run the applet (normally started by the panel)
    cosmic-whispr --toggle        Start recording, or stop and transcribe
    cosmic-whispr --start         Start recording
    cosmic-whispr --stop          Stop recording and transcribe
    cosmic-whispr --cancel        Stop recording and discard
    cosmic-whispr --list-devices  Print available input devices
    cosmic-whispr --check         Report configuration and capabilities
    cosmic-whispr --type-test     Type a test phrase into the focused window
    cosmic-whispr --help          Show this message

SETTING THE API KEY:
    cosmic-whispr --set-key             Read the key from stdin, save it
    cosmic-whispr --set-key-from [REF]  Import it from 1Password, save it
    cosmic-whispr --clear-key           Forget the saved key

DELIVERY:
    --toggle, --start and --stop take --clipboard or --type, choosing where
    the transcript goes. Typing into the focused window is the default;
    --clipboard needs wl-clipboard installed.

Bind --toggle to a keyboard shortcut in COSMIC Settings: clicking the panel
icon moves keyboard focus to the panel, while a shortcut leaves focus in the
window you are dictating into.

Binding a second shortcut to

    cosmic-whispr --toggle --clipboard

gives you a key for each destination: that one copies instead of typing, for
dictating somewhere keystrokes would go to the wrong place. The press that
starts a recording picks the mode.

The API key lives in the Secret Service keyring, and nowhere else. Paste it
into the settings popup, or pipe it in so it never reaches your shell history:

    cosmic-whispr --set-key < key.txt

To take it from 1Password instead, give the reference and the key is fetched
once and saved to the keyring — `op` never runs while you are dictating:

    cosmic-whispr --set-key-from op://Private/openai-api/credential

With no reference, the one in the applet settings is used, so re-importing
after a rotation is just:

    cosmic-whispr --set-key-from

$COSMIC_WHISPR_API_KEY is consulted only when the keyring has nothing to give,
so the applet still works on a machine with no Secret Service running. The
keyring always wins when it holds a key.
";

fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("COSMIC_WHISPR_LOG")
                .unwrap_or_else(|_| EnvFilter::new("warn,cosmic_whispr=info")),
        )
        .init();

    let mut arguments = std::env::args().skip(1);
    let first = arguments.next();
    match first.as_deref() {
        None => cosmic::applet::run::<app::Whispr>(()),
        Some("--toggle") => send(ipc::Command::Toggle(delivery(&mut arguments))),
        Some("--start") => send(ipc::Command::Start(delivery(&mut arguments))),
        Some("--stop") => send(ipc::Command::Stop(delivery(&mut arguments))),
        Some("--cancel") => {
            no_more(&mut arguments);
            send(ipc::Command::Cancel)
        }
        Some("--check") => {
            no_more(&mut arguments);
            check();
            Ok(())
        }
        Some("--set-key") => {
            no_more(&mut arguments);
            set_key()
        }
        Some("--set-key-from") => {
            let reference = arguments.next();
            no_more(&mut arguments);
            set_key_from(reference)
        }
        Some("--clear-key") => {
            no_more(&mut arguments);
            clear_key()
        }
        Some("--type-test") => {
            no_more(&mut arguments);
            type_test();
            Ok(())
        }
        Some("--list-devices") => {
            no_more(&mut arguments);
            for name in audio::input_devices() {
                println!("{name}");
            }
            Ok(())
        }
        Some("--help" | "-h") => {
            print!("{HELP}");
            Ok(())
        }
        Some(unknown) => {
            eprintln!("cosmic-whispr: unknown argument {unknown:?}\n");
            print!("{HELP}");
            std::process::exit(2);
        }
    }
}

/// Reject anything left on the command line.
///
/// Applied to every subcommand, not just the ones that take a flag: the same
/// typo should not be fatal in one place and silently ignored in another.
fn no_more(arguments: &mut impl Iterator<Item = String>) {
    if let Some(extra) = arguments.next() {
        eprintln!("cosmic-whispr: unexpected argument {extra:?}\n");
        print!("{HELP}");
        std::process::exit(2);
    }
}

/// Base of the per-user config tree, the way cosmic-config resolves it.
fn config_home() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })
}

/// Read the delivery flag that may follow a control argument.
///
/// Absent means "whatever the recording already chose", which is what lets
/// the two presses of a toggle disagree harmlessly.
fn delivery(arguments: &mut impl Iterator<Item = String>) -> Option<ipc::Delivery> {
    let delivery = match arguments.next().as_deref() {
        None => None,
        Some("--clipboard") => Some(ipc::Delivery::Clipboard),
        Some("--type") => Some(ipc::Delivery::Type),
        Some(unknown) => {
            eprintln!("cosmic-whispr: expected --clipboard or --type, got {unknown:?}\n");
            print!("{HELP}");
            std::process::exit(2);
        }
    };

    // Say so rather than ignoring it: a dropped argument looks like it
    // worked, which is the worst way for a typo to behave.
    no_more(arguments);

    delivery
}

/// Report what the applet would do, without recording or typing anything.
fn check() {
    let (_, configuration) = config::WhisprConfig::load();

    println!("endpoint:    {}", configuration.transcription_url());
    if !configuration.endpoint_may_carry_key() {
        println!(
            "             NOT HTTPS OR LOOPBACK — the API key is withheld from this endpoint"
        );
    }
    println!("model:       {}", configuration.model);
    println!(
        "cleanup:     {}",
        if configuration.cleanup {
            format!(
                "{} via {}",
                configuration.cleanup_model,
                configuration.chat_url()
            )
        } else {
            "off".to_string()
        }
    );

    // Asked once and used for both lines below: each lookup is its own D-Bus
    // round trip, and on a locked keyring each one is its own unlock prompt.
    let stored = secret::load();
    println!("api key:     {}", secret::Status::from(&stored).describe());
    if !configuration.op_reference.is_empty() {
        // Deliberately not "last imported from": this field has a default, so
        // a fresh install would be told about an import it never did.
        println!("1password:   reference {}", configuration.op_reference);
    }

    let in_use = match stored {
        Ok(Some(key)) => Some((key, "the keyring".to_string())),
        _ => config::key_from_environment(),
    };
    match (in_use, configuration.endpoint_may_carry_key()) {
        (Some((key, source)), true) => {
            println!("in use:      {} chars, from {source}", key.len())
        }
        (Some((_, source)), false) => println!(
            "in use:      none — a key from {source} is held back from a non-HTTPS endpoint"
        ),
        (None, _) => println!("in use:      NOT SET — see --help"),
    }

    // An older version also kept the key in the config itself. Nothing reads
    // that file now, and nothing deleted it, so a plain-text key can be
    // sitting there while this build claims the key lives in the keyring and
    // nowhere else. Say so; deleting a file we no longer own is not ours.
    if let Some(stale) = config_home()
        .map(|base| {
            base.join("cosmic")
                .join(config::APP_ID)
                .join(format!("v{}", config::CONFIG_VERSION))
                .join("api_key")
        })
        .filter(|path| path.exists())
    {
        println!(
            "warning:     {} still holds a plain-text key — delete it, and treat that key as burned",
            stale.display()
        );
    }

    // The key used to live here too. Say so rather than importing it
    // silently: the file may be shared with other tools, so removing it is
    // not ours.
    let legacy = config_home().map(|base| base.join("cosmic-whispr").join(".env"));
    if let Some(path) = legacy.filter(|path| path.exists()) {
        println!(
            "note:        {} is no longer read — import it with --set-key and delete it",
            path.display()
        );
    }

    let device =
        (!configuration.input_device.is_empty()).then_some(configuration.input_device.as_str());
    match audio::describe_input(device) {
        Ok(description) => println!("microphone:  {description}"),
        Err(error) => println!("microphone:  unavailable — {error:#}"),
    }

    if clipboard::is_available() {
        println!("clipboard:   wl-copy available");
    } else {
        println!("clipboard:   UNAVAILABLE — install wl-clipboard to use --clipboard");
    }

    if typer::is_available() {
        println!("typing:      zwp_virtual_keyboard_v1 available");
    } else {
        println!(
            "typing:      UNAVAILABLE — the compositor does not offer zwp_virtual_keyboard_v1"
        );
    }

    let socket = ipc::socket_path();
    match std::os::unix::net::UnixStream::connect(&socket) {
        Ok(_) => println!("applet:      running, listening on {}", socket.display()),
        Err(_) => println!(
            "applet:      not running ({} is not accepting)",
            socket.display()
        ),
    }
}

/// Save a key read from stdin, so it never appears in argv, where it would
/// land in the shell history and in `ps`.
fn set_key() -> cosmic::iced::Result {
    use std::io::Read;


    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!("Paste the API key, then press Enter and Ctrl-D:");
    }

    // A key is a couple of hundred bytes. Reading without a bound means a
    // mistyped redirect — `--set-key < /dev/urandom` — eats memory until it
    // is killed, and every byte of it would be a byte we then try to store.
    const MOST_A_KEY_COULD_BE: u64 = 8 * 1024;

    let mut key = zeroize::Zeroizing::new(String::new());
    if let Err(error) = std::io::stdin()
        .take(MOST_A_KEY_COULD_BE)
        .read_to_string(&mut key)
    {
        eprintln!("cosmic-whispr: cannot read the key from stdin: {error}");
        std::process::exit(1);
    }

    finish(secret::store(&key), "saved the API key to the keyring")
}

/// Pull the key out of 1Password once and save it. The secret goes straight
/// from `op` to the keyring without passing through the terminal.
///
/// With no reference given, the one in the settings is used, so re-importing
/// after a rotation is just `--set-key-from`.
fn set_key_from(reference: Option<String>) -> cosmic::iced::Result {
    let reference = reference.unwrap_or_else(|| {
        let (_, configuration) = config::WhisprConfig::load();
        configuration.op_reference
    });

    if reference.trim().is_empty() {
        eprintln!("cosmic-whispr: --set-key-from needs an op://vault/item/field reference");
        std::process::exit(2);
    }

    finish(
        secret::import_reference(&reference),
        "imported the API key from 1Password into the keyring",
    )
}

fn clear_key() -> cosmic::iced::Result {
    finish(secret::clear(), "removed the API key from the keyring")
}

fn finish(result: anyhow::Result<()>, success: &str) -> cosmic::iced::Result {
    match result {
        Ok(()) => {
            println!("{success}");
            Ok(())
        }
        Err(error) => {
            eprintln!("cosmic-whispr: {error:#}");
            std::process::exit(1);
        }
    }
}

/// Types a known phrase so keystroke delivery can be checked separately from
/// the microphone and the network.
fn type_test() {
    const PHRASE: &str = "cosmic-whispr type test 0123456789";
    const COUNTDOWN: u64 = 3;

    println!("Focus the window you want to type into…");
    for remaining in (1..=COUNTDOWN).rev() {
        println!("  {remaining}");
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    match typer::type_text(PHRASE, std::time::Duration::from_millis(4)) {
        Ok(()) => println!("sent {:?}", PHRASE),
        Err(error) => {
            eprintln!("cosmic-whispr: {error:#}");
            std::process::exit(1);
        }
    }
}

fn send(command: ipc::Command) -> cosmic::iced::Result {
    if let Err(error) = ipc::send(command) {
        eprintln!("cosmic-whispr: {error:#}");
        std::process::exit(1);
    }
    Ok(())
}
