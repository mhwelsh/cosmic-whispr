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
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cosmic::iced::Subscription;
use cosmic::iced::futures::SinkExt;
use cosmic::iced::stream;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Longest command we will read. The longest real one is "toggle clipboard".
const LONGEST_COMMAND: u64 = 64;

/// How long to wait before trying the socket again, and the ceiling that
/// backoff climbs to. A failure here is often transient — the runtime
/// directory not created yet at login, a stale directory from a previous
/// boot — and parking forever would mean every keyboard shortcut is dead for
/// the session over something that fixed itself seconds later.
const RETRY_DELAY: Duration = Duration::from_secs(1);
const RETRY_CEILING: Duration = Duration::from_secs(30);

/// A client that connects and then says nothing gets this long to speak.
///
/// Connections are read one at a time, on purpose. Reading each on its own
/// task removes head-of-line blocking but gives up ordering: `send` writes
/// only after `connect` returns, so two clients racing between those two
/// steps can reach the channel in the wrong order, and a `stop` overtaking
/// its `start` is dropped as "not recording" while the recording it should
/// have ended runs on. Ordering is worth more here than the delay a
/// misbehaving peer can impose, so the read stays inline and this timeout
/// bounds what that peer can cost — a real client writes immediately.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Where a finished transcript goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Delivery {
    /// Synthesize keystrokes into whatever window has focus.
    #[default]
    Type,
    /// Put it on the clipboard and leave the focused window alone, for
    /// dictating somewhere that typing into would be wrong — a password
    /// field, a terminal, someone else's chat window.
    Clipboard,
}

impl Delivery {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "type" => Some(Self::Type),
            "clipboard" => Some(Self::Clipboard),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Clipboard => "clipboard",
        }
    }
}

/// A command from a keyboard shortcut.
///
/// The delivery is optional so that the two presses of a toggle need not
/// agree: the press that starts a recording chooses where the transcript
/// goes, and a later press only changes that if it says so explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Record if idle, otherwise stop and transcribe.
    Toggle(Option<Delivery>),
    Start(Option<Delivery>),
    Stop(Option<Delivery>),
    /// Stop and discard, for when you misspeak.
    Cancel,
}

impl Command {
    /// `"toggle"`, or `"toggle clipboard"`. A bare verb keeps working, which
    /// is what a shortcut bound before delivery modes existed still sends.
    pub fn parse(line: &str) -> Option<Self> {
        let mut words = line.split_whitespace();
        let verb = words.next()?;
        let delivery = match words.next() {
            Some(word) => Some(Delivery::parse(word)?),
            None => None,
        };
        if words.next().is_some() {
            return None;
        }

        match verb {
            "toggle" => Some(Self::Toggle(delivery)),
            "start" => Some(Self::Start(delivery)),
            "stop" => Some(Self::Stop(delivery)),
            "cancel" if delivery.is_none() => Some(Self::Cancel),
            _ => None,
        }
    }

    pub fn encode(self) -> String {
        let (verb, delivery) = match self {
            Self::Toggle(delivery) => ("toggle", delivery),
            Self::Start(delivery) => ("start", delivery),
            Self::Stop(delivery) => ("stop", delivery),
            Self::Cancel => ("cancel", None),
        };
        match delivery {
            Some(delivery) => format!("{verb} {}", delivery.as_str()),
            None => verb.to_string(),
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
    socket_dir_in(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
}

/// The choice itself, taking the runtime directory rather than reading it.
/// Concurrent `setenv` is a data race on the `environ` array — the reason
/// Rust 2024 made it `unsafe` — and test threads share one process, so no
/// test here touches the environment.
fn socket_dir_in(runtime_dir: Option<PathBuf>) -> PathBuf {
    runtime_dir.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("cosmic-whispr-{}", rustix::process::getuid().as_raw()))
    })
}

/// File name of the control socket inside [`socket_dir`].
const SOCKET_NAME: &str = "cosmic-whispr.sock";

pub fn socket_path() -> PathBuf {
    socket_dir().join(SOCKET_NAME)
}

/// Create the socket directory if it is ours to create, and refuse to use one
/// that anybody else could reach into.
///
/// Checked rather than assumed: `create_dir_all` succeeds on a directory that
/// already exists, which an attacker who got there first would own.
fn prepare_socket_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    // Created with the mode already set. Creating and then chmod-ing leaves
    // a window at the umask's 0755, which is exactly long enough for someone
    // to walk in. 0700 survives any umask, which can only clear bits.
    if std::fs::symlink_metadata(dir).is_err() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }

    verify_socket_dir(dir)
}

/// The checks alone, without creating anything.
///
/// The client needs these too. A listener that refuses a directory someone
/// else owns, paired with a client that connects to it anyway, is worse than
/// either alone: the applet declines to bind, the squatter's socket answers
/// instead, and `--toggle` reports success while the shortcut quietly does
/// nothing and every press is delivered to them.
fn verify_socket_dir(dir: &Path) -> Result<()> {
    // `symlink_metadata` rather than `metadata`: the latter follows links, so
    // a symlink planted at our path pointing at some 0700 directory we own —
    // ~/.ssh, say — would pass both checks below and we would bind the
    // control socket inside it.
    let metadata = std::fs::symlink_metadata(dir)
        .with_context(|| format!("cannot inspect {}", dir.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("{} is a symlink, which is not where a control socket belongs", dir.display());
    }
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
    let dir = socket_dir();
    verify_socket_dir(&dir)
        .with_context(|| format!("refusing to use the control socket in {}", dir.display()))?;

    let path = dir.join(SOCKET_NAME);
    let mut stream = std::os::unix::net::UnixStream::connect(&path).with_context(|| {
        format!(
            "no applet listening on {} — is cosmic-whispr running in the panel?",
            path.display()
        )
    })?;
    stream
        .write_all(format!("{}\n", command.encode()).as_bytes())
        .context("cannot send the command")?;
    stream.flush().context("cannot flush the command")
}

/// Listen for commands for as long as the applet runs.
pub fn listen() -> Subscription<Command> {
    Subscription::run(|| {
        stream::channel(
            8,
            |mut output: cosmic::iced::futures::channel::mpsc::Sender<Command>| async move {
                // Retried rather than given up on, and backed off rather
                // than spun: returning would have iced restart the
                // subscription in a tight loop.
                let mut delay = RETRY_DELAY;
                let (listener, path) = loop {
                    match bind_socket() {
                        Ok(bound) => break bound,
                        Err(error) => {
                            tracing::warn!(
                                "control socket unavailable, retrying in {}s: {error:#}",
                                delay.as_secs()
                            );
                            tokio::time::sleep(delay).await;
                            delay = (delay * 2).min(RETRY_CEILING);
                        }
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
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            },
        )
    })
}

/// Prepare the directory and bind the socket inside it.
fn bind_socket() -> Result<(UnixListener, PathBuf)> {
    let dir = socket_dir();
    prepare_socket_dir(&dir)?;

    let path = dir.join(SOCKET_NAME);
    // A socket left behind by a crashed instance would block bind.
    let _ = std::fs::remove_file(&path);

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("cannot bind {}", path.display()))?;

    // The directory already keeps others out; narrowing the socket itself
    // means a permissive umask cannot widen it either.
    if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(path = %path.display(), %error, "cannot restrict the control socket");
    }

    Ok((listener, path))
}

async fn read_command(connection: UnixStream) -> Option<Command> {
    let mut line = String::new();
    let mut reader = BufReader::new(connection.take(LONGEST_COMMAND));

    match tokio::time::timeout(READ_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(0)) => None,
        Ok(Ok(_)) => {
            let command = Command::parse(&line);
            if command.is_none() {
                tracing::warn!(?line, "unknown control command");
            }
            command
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "cannot read from the control socket");
            None
        }
        Err(_) => {
            tracing::warn!("a control connection sent nothing; dropping it");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_commands() {
        assert_eq!(Command::parse("toggle\n"), Some(Command::Toggle(None)));
        assert_eq!(Command::parse("  cancel  "), Some(Command::Cancel));
        assert_eq!(Command::parse("explode"), None);
    }

    #[test]
    fn parses_a_delivery_mode() {
        assert_eq!(
            Command::parse("toggle clipboard"),
            Some(Command::Toggle(Some(Delivery::Clipboard)))
        );
        assert_eq!(
            Command::parse("stop type\n"),
            Some(Command::Stop(Some(Delivery::Type)))
        );
    }

    #[test]
    fn rejects_nonsense_around_a_known_verb() {
        assert_eq!(Command::parse("toggle sideways"), None);
        assert_eq!(Command::parse("toggle clipboard extra"), None);
        assert_eq!(Command::parse("cancel clipboard"), None);
        assert_eq!(Command::parse(""), None);
    }

    #[test]
    fn commands_round_trip() {
        for command in [
            Command::Toggle(None),
            Command::Toggle(Some(Delivery::Clipboard)),
            Command::Start(Some(Delivery::Type)),
            Command::Stop(Some(Delivery::Clipboard)),
            Command::Cancel,
        ] {
            assert_eq!(Command::parse(&command.encode()), Some(command));
        }
    }

    #[test]
    fn socket_path_follows_the_runtime_dir() {
        assert_eq!(
            socket_dir_in(Some(PathBuf::from("/run/user/1234"))),
            PathBuf::from("/run/user/1234")
        );

        // Without it, the socket must not land directly in a world-writable
        // temp directory, where anyone could connect and drive the applet.
        let fallback = socket_dir_in(None);
        assert_ne!(fallback, std::env::temp_dir());
        assert!(fallback.starts_with(std::env::temp_dir()), "{fallback:?}");
        assert!(
            fallback
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("cosmic-whispr-")),
            "{fallback:?}"
        );
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
    fn prepare_refuses_a_symlink_even_to_somewhere_we_own() {
        let target = std::env::temp_dir().join(format!(
            "cosmic-whispr-test-target-{}",
            std::process::id()
        ));
        let link = std::env::temp_dir().join(format!(
            "cosmic-whispr-test-link-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&target);
        let _ = std::fs::remove_file(&link);

        // A private directory we own, which is what makes this the
        // interesting case: the uid and mode checks would both pass.
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&target)
            .expect("create target");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let error = prepare_socket_dir(&link).unwrap_err().to_string();
        assert!(error.contains("symlink"), "{error}");

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn prepare_creates_it_private_from_the_first_moment() {
        let dir = std::env::temp_dir().join(format!(
            "cosmic-whispr-test-mode-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        prepare_socket_dir(&dir).expect("creates");
        // Set at creation rather than chmod-ed afterwards, so there is no
        // window at the umask's default.
        let mode = std::fs::symlink_metadata(&dir)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{mode:04o}");

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
