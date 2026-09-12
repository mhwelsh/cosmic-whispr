// SPDX-License-Identifier: MPL-2.0

//! Reads the API key out of an env file.
//!
//! The file is ordinary `KEY=value` dotenv syntax. A value may be a literal
//! secret, or a `op://vault/item/field` reference, which is resolved by
//! shelling out to the 1Password CLI — that is the shape 1Password's env-file
//! storage produces, so the same file works whether the applet reads it
//! directly or is launched under `op run`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

/// Variable names looked for in the env file, in order. `OPENAI_API_KEY` is
/// included because that is what an env file shared with other tools is
/// likely to already call it.
pub const KEY_NAMES: [&str; 2] = ["COSMIC_WHISPR_API_KEY", "OPENAI_API_KEY"];

/// 1Password secret references start with this scheme.
const REFERENCE_SCHEME: &str = "op://";

/// `op read` can block on a biometric prompt. Give up rather than leave the
/// applet stuck in "Transcribing…" forever.
const OP_TIMEOUT: Duration = Duration::from_secs(30);

pub fn default_env_file() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("cosmic-whispr").join(".env")
}

/// Read the key from `path`, resolving a 1Password reference if that is what
/// the value turns out to be. Returns the key and a description of where it
/// came from, for `--check` to print without revealing the secret.
pub fn read_key(path: &Path) -> Result<(String, String)> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the env file {}", path.display()))?;

    warn_if_readable_by_others(path);

    let (name, value) = lookup(&contents, &KEY_NAMES).ok_or_else(|| {
        anyhow!(
            "{} defines none of {}",
            path.display(),
            KEY_NAMES.join(", ")
        )
    })?;

    if is_reference(&value) {
        let key = resolve_reference(&value)
            .with_context(|| format!("cannot resolve {name} from 1Password"))?;
        return Ok((key, format!("{name} in {} via 1Password", path.display())));
    }

    Ok((value, format!("{name} in {}", path.display())))
}

pub fn is_reference(value: &str) -> bool {
    value.starts_with(REFERENCE_SCHEME)
}

/// First of `names` that the env file defines with a non-empty value.
pub fn lookup(contents: &str, names: &[&str]) -> Option<(String, String)> {
    let entries = parse(contents);
    names.iter().find_map(|name| {
        entries
            .iter()
            .find(|(key, value)| key == name && !value.is_empty())
            .map(|(key, value)| (key.clone(), value.clone()))
    })
}

/// Parse dotenv syntax: `KEY=value`, optionally `export`-prefixed, with an
/// optionally quoted value. Malformed lines are skipped rather than fatal —
/// the file is shared with other tools and may hold things we do not model.
pub fn parse(contents: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };

        let name = name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }

        entries.push((name.to_string(), unquote(value.trim()).to_string()));
    }

    entries
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Resolved references, memoized for the life of the process.
///
/// Without this, every dictation shells out to `op` again, and a 1Password
/// setup that prompts for biometrics would prompt on every sentence. The cost
/// is that a secret rotated in 1Password is only picked up after a restart.
static RESOLVED: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Ask the 1Password CLI for the secret behind a `op://` reference.
fn resolve_reference(reference: &str) -> Result<String> {
    if let Ok(cache) = RESOLVED.lock()
        && let Some((cached_reference, key)) = cache.as_ref()
        && cached_reference == reference
    {
        return Ok(key.clone());
    }

    let key = read_reference(reference)?;
    if let Ok(mut cache) = RESOLVED.lock() {
        *cache = Some((reference.to_string(), key.clone()));
    }
    Ok(key)
}

fn read_reference(reference: &str) -> Result<String> {
    let mut child = Command::new("op")
        .arg("read")
        .arg(reference)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => anyhow!(
                "the 1Password CLI (`op`) is not installed, but the env file holds \
                 a {REFERENCE_SCHEME} reference"
            ),
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

    let key = String::from_utf8(output.stdout)
        .context("`op read` returned invalid UTF-8")?
        .trim()
        .to_string();
    if key.is_empty() {
        bail!("`op read` returned an empty value");
    }
    Ok(key)
}

/// A secrets file should not be group- or world-readable. Warn rather than
/// refuse: the file may be a symlink into a directory that is already locked
/// down, and refusing to dictate over a permission bit helps nobody.
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        tracing::warn!(
            path = %path.display(),
            mode = format!("{mode:04o}"),
            "env file is readable by other users; consider chmod 600",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_assignments() {
        let entries = parse("FOO=bar\nBAZ=qux\n");
        assert_eq!(
            entries,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn ignores_comments_blank_lines_and_junk() {
        let entries = parse("# a comment\n\n   \nnot an assignment\nFOO=bar\n");
        assert_eq!(entries, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn handles_export_prefix_and_quotes() {
        let entries = parse("export FOO=\"bar\"\nBAZ='qux'\nBARE = spaced \n");
        assert_eq!(entries[0], ("FOO".to_string(), "bar".to_string()));
        assert_eq!(entries[1], ("BAZ".to_string(), "qux".to_string()));
        assert_eq!(entries[2], ("BARE".to_string(), "spaced".to_string()));
    }

    #[test]
    fn keeps_characters_that_appear_in_api_keys() {
        let entries = parse("OPENAI_API_KEY=sk-proj-aB3/d+f=g_h-i\n");
        assert_eq!(entries[0].1, "sk-proj-aB3/d+f=g_h-i");
    }

    #[test]
    fn lookup_prefers_the_first_name_given() {
        let contents = "OPENAI_API_KEY=second\nCOSMIC_WHISPR_API_KEY=first\n";
        let (name, value) = lookup(contents, &KEY_NAMES).expect("found");
        assert_eq!(name, "COSMIC_WHISPR_API_KEY");
        assert_eq!(value, "first");
    }

    #[test]
    fn lookup_falls_through_to_openai_name() {
        let (name, value) = lookup("OPENAI_API_KEY=sk-test\n", &KEY_NAMES).expect("found");
        assert_eq!(name, "OPENAI_API_KEY");
        assert_eq!(value, "sk-test");
    }

    #[test]
    fn lookup_skips_empty_values() {
        assert!(lookup("COSMIC_WHISPR_API_KEY=\n", &KEY_NAMES).is_none());
        let (name, _) =
            lookup("COSMIC_WHISPR_API_KEY=\nOPENAI_API_KEY=sk\n", &KEY_NAMES).expect("found");
        assert_eq!(name, "OPENAI_API_KEY");
    }

    #[test]
    fn resolved_references_are_memoized() {
        // `op` is not run here: seeding the cache is enough to prove the
        // lookup short-circuits before spawning anything.
        let reference = "op://Test/Memoized/credential";
        *RESOLVED.lock().expect("lock") = Some((reference.to_string(), "sk-cached".to_string()));
        assert_eq!(resolve_reference(reference).expect("cached"), "sk-cached");
        *RESOLVED.lock().expect("lock") = None;
    }

    #[test]
    fn recognizes_1password_references() {
        assert!(is_reference("op://Private/OpenAI/credential"));
        assert!(!is_reference("sk-proj-literal"));
    }

    #[test]
    fn read_key_reports_a_missing_variable() {
        let path = std::env::temp_dir().join("cosmic-whispr-empty.env");
        std::fs::write(&path, "UNRELATED=1\n").expect("write");
        let error = read_key(&path).unwrap_err().to_string();
        assert!(error.contains("defines none of"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_key_returns_a_literal_value() {
        let path = std::env::temp_dir().join("cosmic-whispr-literal.env");
        std::fs::write(&path, "# key\nOPENAI_API_KEY=\"sk-literal\"\n").expect("write");
        let (key, source) = read_key(&path).expect("reads");
        assert_eq!(key, "sk-literal");
        assert!(source.contains("OPENAI_API_KEY"), "{source}");
        assert!(!source.contains("1Password"), "{source}");
        let _ = std::fs::remove_file(&path);
    }
}
