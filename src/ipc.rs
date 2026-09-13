// SPDX-License-Identifier: MPL-2.0

//! A tiny control socket so a keyboard shortcut can drive the applet.
//!
//! Clicking the panel icon moves keyboard focus to the panel, which is
//! exactly the wrong thing for a tool that types into the focused window.
//! Binding `cosmic-whispr --toggle` to a shortcut in COSMIC Settings keeps
//! focus where the user is actually typing.

use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
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

/// Directory the control socket lives in.
///
/// `$XDG_RUNTIME_DIR` is per-user and mode 0700 by specification, so a socket
/// inside it is unreachable by other users whatever its own mode. Without it,
/// the fallback must not be bare `/tmp`: a socket there is created under the
/// umask, and the usual 0755 lets any local user connect and drive the applet
/// — starting the microphone and typing into the focused window. Fall back to
/// a per-uid directory instead, and make it private ourselves.
pub fn socket_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("cosmic-whispr-{}", rustix::process::getuid().as_raw()))
        })
}

pub fn socket_path() -> PathBuf {
    socket_dir().join("cosmic-whispr.sock")
}

/// Create the socket directory if it is ours to create, and refuse to use one
/// that anybody else could reach into.
///
/// Checked rather than assumed: `create_dir_all` succeeds on a directory that
/// already exists, which an attacker who got there first would own.
fn prepare_socket_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot restrict {}", dir.display()))?;
    }

    let metadata = std::fs::metadata(dir)
        .with_context(|| format!("cannot inspect {}", dir.display()))?;
    if !metadata.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    let uid = rustix::process::getuid().as_raw();
    if metadata.uid() != uid {
        bail!(
            "{} belongs to uid {}, not {uid}",
            dir.display(),
            metadata.uid()
        );
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} is mode {mode:04o}; a control socket there would be reachable by other users",
            dir.display()
        );
    }

    Ok(())
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
                let dir = socket_dir();
                if let Err(error) = prepare_socket_dir(&dir) {
                    tracing::warn!("control socket unavailable: {error:#}");
                    // Stay pending rather than ending the subscription, which
                    // iced would otherwise restart in a tight loop.
                    std::future::pending::<()>().await;
                    unreachable!();
                }

                let path = dir.join("cosmic-whispr.sock");
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
                // The directory already keeps others out; narrowing the socket
                // itself means a permissive umask cannot widen it either.
                if let Err(error) =
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                {
                    tracing::warn!(path = %path.display(), %error, "cannot restrict the control socket");
                }

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

        // Without it, the socket must not land directly in a world-writable
        // temp directory, where anyone could connect and drive the applet.
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
        let fallback = socket_dir();
        assert_ne!(fallback, std::env::temp_dir());
        assert!(fallback.starts_with(std::env::temp_dir()), "{fallback:?}");
        assert!(
            fallback
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("cosmic-whispr-")),
            "{fallback:?}"
        );

        match previous {
            Some(value) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", value) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
    }

    #[test]
    fn prepare_creates_a_private_directory() {
        let dir = std::env::temp_dir().join(format!(
            "cosmic-whispr-test-private-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        prepare_socket_dir(&dir).expect("creates");
        let mode = std::fs::metadata(&dir).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{mode:04o}");

        // Running twice is how every restart goes.
        prepare_socket_dir(&dir).expect("accepts its own directory");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_refuses_a_directory_others_can_reach_into() {
        let dir = std::env::temp_dir().join(format!(
            "cosmic-whispr-test-open-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).expect("chmod");

        let error = prepare_socket_dir(&dir).unwrap_err().to_string();
        assert!(error.contains("0777"), "{error}");
        assert!(error.contains("other users"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
