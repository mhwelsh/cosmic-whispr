// SPDX-License-Identifier: MPL-2.0

//! cosmic-whispr — dictation for the COSMIC panel.
//!
//! Records from the microphone, reduces the audio to 16 kHz mono PCM, sends
//! it to an OpenAI-compatible transcription endpoint, and types the result
//! into whatever window has keyboard focus.

mod app;
mod audio;
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

Bind --toggle to a keyboard shortcut in COSMIC Settings: clicking the panel
icon moves keyboard focus to the panel, while a shortcut leaves focus in the
window you are dictating into.

The API key lives in the Secret Service keyring, and nowhere else. Paste it
into the settings popup, or pipe it in so it never reaches your shell history:

    cosmic-whispr --set-key < key.txt

To take it from 1Password instead, give the reference and the key is fetched
once and saved to the keyring — `op` never runs while you are dictating:

    cosmic-whispr --set-key-from op://Private/OpenAI/credential

With no reference, the one in the applet settings is used, so re-importing
after a rotation is just:

    cosmic-whispr --set-key-from

$COSMIC_WHISPR_API_KEY (or $OPENAI_API_KEY) overrides the keyring when set,
which is for trying a throwaway key without disturbing the saved one.
";

fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("COSMIC_WHISPR_LOG")
                .unwrap_or_else(|_| EnvFilter::new("warn,cosmic_whispr=info")),
        )
        .init();

    let mut arguments = std::env::args().skip(1);
    match arguments.next().as_deref() {
        None => {
            // An older version kept the key in the config file; move it into
            // the keyring before the applet can read either.
            if let (Some(handle), _) = config::WhisprConfig::load() {
                config::WhisprConfig::migrate_plaintext_key(&handle);
            }
            cosmic::applet::run::<app::Whispr>(())
        }
        Some("--toggle") => send(ipc::Command::Toggle),
        Some("--start") => send(ipc::Command::Start),
        Some("--stop") => send(ipc::Command::Stop),
        Some("--cancel") => send(ipc::Command::Cancel),
        Some("--check") => {
            check();
            Ok(())
        }
        Some("--set-key") => set_key(),
        Some("--set-key-from") => set_key_from(arguments.next()),
        Some("--clear-key") => clear_key(),
        Some("--type-test") => {
            type_test();
            Ok(())
        }
        Some("--list-devices") => {
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

/// Report what the applet would do, without recording or typing anything.
fn check() {
    let (_, configuration) = config::WhisprConfig::load();

    println!("endpoint:    {}", configuration.transcription_url());
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

    println!("api key:     {}", secret::status().describe());
    if !configuration.op_reference.is_empty() {
        println!("1password:   last imported from {}", configuration.op_reference);
    }

    match configuration.api_key_with_source() {
        Some((key, source)) => println!("in use:      {} chars, from {source}", key.len()),
        None => println!("in use:      NOT SET — see --help"),
    }

    // The key used to live here. Say so rather than importing it silently:
    // the file may be shared with other tools, so removing it is not ours.
    let legacy = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config")))
        .map(|base| base.join("cosmic-whispr").join(".env"));
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

    let mut key = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut key) {
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
