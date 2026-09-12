// SPDX-License-Identifier: MPL-2.0

//! cosmic-whispr — dictation for the COSMIC panel.
//!
//! Records from the microphone, reduces the audio to 16 kHz mono PCM, sends
//! it to an OpenAI-compatible transcription endpoint, and types the result
//! into whatever window has keyboard focus.

mod app;
mod audio;
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

Bind --toggle to a keyboard shortcut in COSMIC Settings: clicking the panel
icon moves keyboard focus to the panel, while a shortcut leaves focus in the
window you are dictating into.

The API key is read from the environment ($COSMIC_WHISPR_API_KEY, then
$OPENAI_API_KEY), then from the env file (~/.config/cosmic-whispr/.env by
default), then from the value stored in the applet settings.

The env file is dotenv syntax. A value may be the key itself, or a
op://vault/item/field reference, which is resolved with the 1Password CLI:

    OPENAI_API_KEY=op://Private/OpenAI/credential
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
        None => cosmic::applet::run::<app::Whispr>(()),
        Some("--toggle") => send(ipc::Command::Toggle),
        Some("--start") => send(ipc::Command::Start),
        Some("--stop") => send(ipc::Command::Stop),
        Some("--cancel") => send(ipc::Command::Cancel),
        Some("--check") => {
            check();
            Ok(())
        }
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

    let env_file = configuration.env_file_path();
    match std::fs::read_to_string(&env_file) {
        Ok(contents) => {
            let found = secret::lookup(&contents, &secret::KEY_NAMES);
            let description = match &found {
                Some((name, value)) if secret::is_reference(value) => {
                    format!("{name} = a 1Password reference")
                }
                Some((name, _)) => format!("{name} = a literal key"),
                None => format!("defines none of {}", secret::KEY_NAMES.join(", ")),
            };
            println!("env file:    {} — {description}", env_file.display());
        }
        Err(error) => println!("env file:    {} — {error}", env_file.display()),
    }

    match configuration.api_key_with_source() {
        Some((key, source)) => println!("api key:     {} chars, from {source}", key.len()),
        None => println!("api key:     NOT SET — see --help"),
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
