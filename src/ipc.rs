// SPDX-License-Identifier: MPL-2.0

//! A tiny control socket so a keyboard shortcut can drive the applet.
//!
//! Clicking the panel icon moves keyboard focus to the panel, which is
//! exactly the wrong thing for a tool that types into the focused window.
//! Binding `cosmic-whispr --toggle` to a shortcut in COSMIC Settings keeps
//! focus where the user is actually typing.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use cosmic::iced::Subscription;
use cosmic::iced::futures::SinkExt;
use cosmic::iced::stream;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Record if idle, otherwise stop and transcribe.
    Toggle,
    Start,
    Stop,
    /// Stop and discard, for when you misspeak.
    Cancel,
}

impl Command {
    pub fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "toggle" => Some(Self::Toggle),
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Toggle => "toggle",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Cancel => "cancel",
        }
    }
}

pub fn socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("cosmic-whispr.sock")
}

/// Send one command to a running applet. Blocking, for the CLI path.
pub fn send(command: Command) -> Result<()> {
    let path = socket_path();
    let mut stream = std::os::unix::net::UnixStream::connect(&path).with_context(|| {
        format!(
            "no applet listening on {} — is cosmic-whispr running in the panel?",
            path.display()
        )
    })?;
    stream
        .write_all(format!("{}\n", command.as_str()).as_bytes())
        .context("cannot send the command")?;
    stream.flush().context("cannot flush the command")
}

/// Listen for commands for as long as the applet runs.
pub fn listen() -> Subscription<Command> {
    Subscription::run(|| {
        stream::channel(
            8,
            |mut output: cosmic::iced::futures::channel::mpsc::Sender<Command>| async move {
                let path = socket_path();
                // A socket left behind by a crashed instance would block bind.
                let _ = std::fs::remove_file(&path);

                let listener = match UnixListener::bind(&path) {
                    Ok(listener) => listener,
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "control socket unavailable");
                        // Stay pending rather than ending the subscription, which
                        // iced would otherwise restart in a tight loop.
                        std::future::pending::<()>().await;
                        unreachable!();
                    }
                };
                tracing::info!(path = %path.display(), "listening for control commands");

                loop {
                    match listener.accept().await {
                        Ok((connection, _)) => {
                            if let Some(command) = read_command(connection).await {
                                let _ = output.send(command).await;
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "control socket accept failed");
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                    }
                }
            },
        )
    })
}

async fn read_command(connection: UnixStream) -> Option<Command> {
    let mut line = String::new();
    match BufReader::new(connection).read_line(&mut line).await {
        Ok(0) => None,
        Ok(_) => {
            let command = Command::parse(&line);
            if command.is_none() {
                tracing::warn!(?line, "unknown control command");
            }
            command
        }
        Err(error) => {
            tracing::warn!(%error, "cannot read from the control socket");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_commands() {
        assert_eq!(Command::parse("toggle\n"), Some(Command::Toggle));
        assert_eq!(Command::parse("  cancel  "), Some(Command::Cancel));
        assert_eq!(Command::parse("explode"), None);
    }

    #[test]
    fn command_names_round_trip() {
        for command in [
            Command::Toggle,
            Command::Start,
            Command::Stop,
            Command::Cancel,
        ] {
            assert_eq!(Command::parse(command.as_str()), Some(command));
        }
    }

    #[test]
    fn socket_path_follows_the_runtime_dir() {
        // SAFETY: single-threaded test, and the value is restored below.
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1234") };
        assert_eq!(
            socket_path(),
            PathBuf::from("/run/user/1234/cosmic-whispr.sock")
        );
        match previous {
            Some(value) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", value) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
    }
}
