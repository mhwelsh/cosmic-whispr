// SPDX-License-Identifier: MPL-2.0

//! The API key, kept in the Secret Service keyring.
//!
//! The keyring is the only place dictation reads the key from. The 1Password
//! CLI appears here too, but only as an import path: [`read_reference`] pulls
//! a key out of 1Password once, during setup, so it can be handed to
//! [`store`]. `op` never runs while a transcript is in flight.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use keyring::Entry;
use zeroize::Zeroizing;

use crate::config::APP_ID;

/// An API key, wiped from memory when the last copy goes away.
///
/// A plain `String` leaves the key in freed heap until something reuses the
/// allocation, which puts it within reach of a core dump or of swap — both
/// disk-at-rest paths, and therefore inside the boundary the keyring is meant
/// to close. `Zeroizing` overwrites the buffer on drop instead.
///
/// The [`Debug`] implementation prints nothing useful, so the key cannot
/// reach a log through a `{:?}` on some struct that happens to contain it.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(Zeroizing<String>);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(Zeroizing::new(key.into()))
    }

    /// Hand out the key itself. Named to make call sites easy to audit.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Character count, for reporting a key without revealing it.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(redacted)")
    }
}

/// Account name the key is filed under. Secret Service items are addressed
/// by a (service, account) pair, and the service is the application ID.
const ACCOUNT: &str = "api-key";

/// 1Password secret references start with this scheme.
const REFERENCE_SCHEME: &str = "op://";

/// `op read` can block on a biometric prompt. This only ever runs from a
/// setup action, but a hung prompt should still not wedge the applet.
const OP_TIMEOUT: Duration = Duration::from_secs(30);

/// Longer than any real key, short enough that a misdirected pipe is caught
/// rather than stored. A current OpenAI project key is 164 characters.
const LONGEST_PLAUSIBLE_KEY: usize = 1024;

/// What the keyring holds, as far as the settings panel needs to know.
///
/// Computed off the UI thread and cached: asking this question means talking
/// to D-Bus, which a `view` must never do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// A key is saved. The length is kept so the panel can show something
    /// specific without revealing the key.
    Stored { length: usize },
    /// The keyring works, but nothing has been saved yet.
    Empty,
    /// No Secret Service to talk to, or it refused.
    Unavailable(String),
}

impl Status {
    /// One line for the settings panel and `--check`.
    pub fn describe(&self) -> String {
        match self {
            Self::Stored { length } => format!("Stored in the keyring — {length} characters"),
            Self::Empty => "No key saved yet".to_string(),
            Self::Unavailable(error) => format!("Keyring unavailable — {error}"),
        }
    }
}

fn entry() -> Result<Entry> {
    Entry::new(APP_ID, ACCOUNT).context("cannot reach the Secret Service keyring")
}

/// Run a keyring operation on a thread of its own.
///
/// The Secret Service client is a synchronous face on an async D-Bus crate:
/// it calls `block_on` internally, which panics outright if it lands on a
/// thread that is currently driving futures. Worse, the store holds a
/// process-global mutex across that call, so a single such panic poisons the
/// keyring for the rest of the run — every later lookup fails too.
///
/// A dedicated thread keeps every caller out of that trap, async or not, for
/// the price of one thread spawn against a D-Bus round trip.
fn isolated<T>(operation: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T>
where
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name("keyring".to_string())
        .spawn(operation)
        .context("cannot start the keyring thread")?
        .join()
        .map_err(|_| anyhow!("the keyring thread panicked"))?
}

/// Save the key, replacing whatever was there.
///
/// Every route in — the settings box, `--set-key`, a 1Password import — ends
/// here, so this is the one place worth checking that what arrived could
/// plausibly be a key. Storing junk silently is the bad outcome: it looks
/// like it worked, and the failure surfaces much later as an authentication
/// error nobody connects back to this moment.
pub fn store(key: &str) -> Result<()> {
    let key = Zeroizing::new(key.trim().to_string());
    if key.is_empty() {
        bail!("the API key is empty");
    }
    if key.len() > LONGEST_PLAUSIBLE_KEY {
        bail!(
            "that is {} characters long, which is not an API key — check what you piped in",
            key.len()
        );
    }
    if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!(
            "the API key contains whitespace, so it is probably more than the key — \
             it should be a single token"
        );
    }

    isolated(move || {
        entry()?
            .set_password(&key)
            .context("cannot save the API key to the keyring")
    })
}

/// Read the key back. `Ok(None)` means the keyring works but holds nothing,
/// which is a setup state rather than a failure.
pub fn load() -> Result<Option<ApiKey>> {
    isolated(|| match entry()?.get_password() {
        Ok(key) => Ok(Some(ApiKey::new(key))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(anyhow!(error).context("cannot read the API key from the keyring")),
    })
}

/// Forget the stored key. Deleting one that is already gone is not an error.
pub fn clear() -> Result<()> {
    isolated(|| match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(anyhow!(error).context("cannot remove the API key from the keyring")),
    })
}

/// Summarize the keyring for display. Never fails: an unreachable keyring is
/// something the panel reports, not something it propagates.
pub fn status() -> Status {
    match load() {
        Ok(Some(key)) => Status::Stored { length: key.len() },
        Ok(None) => Status::Empty,
        Err(error) => Status::Unavailable(format!("{error:#}")),
    }
}

pub fn is_reference(value: &str) -> bool {
    value.trim().starts_with(REFERENCE_SCHEME)
}

/// Resolve an `op://vault/item/field` reference with the 1Password CLI.
///
/// Setup only. The result is meant to go straight into [`store`]; nothing
/// caches it, because nothing calls this twice.
pub fn read_reference(reference: &str) -> Result<Zeroizing<String>> {
    let reference = reference.trim();
    if !is_reference(reference) {
        bail!("a 1Password reference looks like {REFERENCE_SCHEME}vault/item/field");
    }

    let mut child = Command::new("op")
        .arg("read")
        .arg(reference)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                anyhow!("the 1Password CLI (`op`) is not installed")
            }
            _ => anyhow!("cannot run `op read`: {error}"),
        })?;

    let deadline = Instant::now() + OP_TIMEOUT;
    loop {
        match child.try_wait().context("cannot wait for `op read`")? {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "`op read` timed out after {} s — is 1Password unlocked and the \
                     CLI integration enabled?",
                    OP_TIMEOUT.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }

    let output = child
        .wait_with_output()
        .context("cannot read the output of `op read`")?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        let message = message.trim();
        bail!(if message.is_empty() {
            format!("`op read` failed with {}", output.status)
        } else {
            message.to_string()
        });
    }

    // The child's stdout buffer holds the secret; wipe it along with the
    // string built from it.
    let stdout = Zeroizing::new(output.stdout);
    let key = Zeroizing::new(
        std::str::from_utf8(&stdout)
            .context("`op read` returned invalid UTF-8")?
            .trim()
            .to_string(),
    );
    if key.is_empty() {
        bail!("`op read` returned an empty value");
    }
    Ok(key)
}

/// Resolve a reference and save the result in one step, so the secret never
/// has to travel back through the caller.
pub fn import_reference(reference: &str) -> Result<()> {
    let key = read_reference(reference)
        .with_context(|| format!("cannot read {} from 1Password", reference.trim()))?;
    store(&key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_1password_references() {
        assert!(is_reference("op://Private/OpenAI/credential"));
        assert!(is_reference("  op://Private/OpenAI/credential  "));
        assert!(!is_reference("sk-proj-literal"));
        assert!(!is_reference(""));
    }

    #[test]
    fn refuses_to_store_an_empty_key() {
        let error = store("   ").unwrap_err().to_string();
        assert!(error.contains("empty"), "{error}");
    }

    #[test]
    fn refuses_to_store_something_that_is_not_a_key() {
        // A misdirected pipe: `--set-key < some-large-file`.
        let error = store(&"x".repeat(LONGEST_PLAUSIBLE_KEY + 1))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an API key"), "{error}");

        // A whole dotenv line, or two keys pasted at once.
        for junk in ["OPENAI_API_KEY = sk-abc", "sk-one\nsk-two", "sk-with\ttab"] {
            let error = store(junk).unwrap_err().to_string();
            assert!(error.contains("whitespace"), "{junk:?} -> {error}");
        }
    }

    #[test]
    fn refuses_a_reference_that_is_not_one() {
        // `op` is never spawned: the scheme check rejects this first.
        let error = read_reference("sk-proj-literal").unwrap_err().to_string();
        assert!(error.contains("vault/item/field"), "{error}");
    }

    #[test]
    fn a_key_never_prints_itself() {
        let key = ApiKey::new("sk-proj-should-not-appear");
        let rendered = format!("{key:?}");
        assert!(!rendered.contains("sk-proj"), "{rendered}");
        assert_eq!(key.expose(), "sk-proj-should-not-appear");
        assert_eq!(key.len(), 25);
    }

    #[test]
    fn status_describes_a_stored_key_without_revealing_it() {
        let described = Status::Stored { length: 164 }.describe();
        assert!(described.contains("164"), "{described}");
        assert_eq!(Status::Empty.describe(), "No key saved yet");
    }

    /// Needs a live Secret Service, so it is not part of the default run.
    #[test]
    #[ignore = "requires a running Secret Service"]
    fn round_trips_through_the_keyring() {
        store("sk-test-round-trip").expect("store");
        assert_eq!(
            load().expect("load").map(|key| key.expose().to_string()),
            Some("sk-test-round-trip".to_string())
        );
        clear().expect("clear");
        assert!(load().expect("load").is_none());
        clear().expect("clearing twice is fine");
    }
}
