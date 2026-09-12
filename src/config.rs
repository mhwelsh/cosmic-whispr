// SPDX-License-Identifier: MPL-2.0

//! Persisted settings, stored through `cosmic-config` so COSMIC Settings and
//! the applet see the same values.

use std::path::PathBuf;

use crate::secret;
use cosmic::cosmic_config::cosmic_config_derive::CosmicConfigEntry;
use cosmic::cosmic_config::{self, Config, CosmicConfigEntry};
use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "dev.mhwelsh.CosmicWhispr";
pub const CONFIG_VERSION: u64 = 1;

/// Environment variables checked before the env file, so launching the
/// applet under `op run` works without any file reading of our own.
pub const API_KEY_ENV: [&str; 2] = secret::KEY_NAMES;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CosmicConfigEntry)]
#[version = 1]
pub struct WhisprConfig {
    /// Base URL of an OpenAI-compatible API, without the trailing slash.
    /// Transcription is posted to `{api_base}/audio/transcriptions`.
    pub api_base: String,
    /// Transcription model name, as the endpoint spells it.
    pub model: String,
    /// ISO-639-1 hint (`"en"`). Empty means let the service auto-detect.
    pub language: String,
    /// Optional biasing prompt: jargon, names, or the desired punctuation style.
    pub prompt: String,
    /// API key stored in the config file. Prefer `env_file` or the
    /// environment.
    pub api_key: String,
    /// Path to a dotenv file defining `COSMIC_WHISPR_API_KEY` or
    /// `OPENAI_API_KEY`. The value may be a literal key or a
    /// `op://vault/item/field` reference for the 1Password CLI to resolve.
    /// `~` is expanded; empty means the default location.
    pub env_file: String,
    /// Input device name as reported by cpal. Empty means the system default.
    pub input_device: String,
    /// Delay between synthesized keystrokes. 0 is fastest; raise it if a
    /// target application drops characters.
    pub type_delay_ms: u64,
    /// Append a space after each transcript, so dictating twice in a row
    /// does not run the words together.
    pub trailing_space: bool,
    /// Hard cap on a single recording, as a runaway-microphone guard.
    pub max_seconds: u64,
}

impl Default for WhisprConfig {
    fn default() -> Self {
        Self {
            api_base: "https://api.openai.com/v1".into(),
            model: "whisper-1".into(),
            language: String::new(),
            prompt: String::new(),
            api_key: String::new(),
            env_file: String::new(),
            input_device: String::new(),
            type_delay_ms: 4,
            trailing_space: true,
            max_seconds: 300,
        }
    }
}

impl WhisprConfig {
    /// Open the config handle, falling back to defaults when the daemon or
    /// config directory is unavailable so the applet still starts.
    pub fn load() -> (Option<Config>, Self) {
        match Config::new(APP_ID, CONFIG_VERSION) {
            Ok(handle) => {
                let config = Self::get_entry(&handle).unwrap_or_else(|(errors, fallback)| {
                    for error in errors {
                        tracing::warn!(%error, "falling back to default for config key");
                    }
                    fallback
                });
                (Some(handle), config)
            }
            Err(error) => {
                tracing::warn!(%error, "cosmic-config unavailable, using defaults");
                (None, Self::default())
            }
        }
    }

    /// Path of the env file to read, falling back to the default location.
    pub fn env_file_path(&self) -> PathBuf {
        if self.env_file.trim().is_empty() {
            secret::default_env_file()
        } else {
            expand_tilde(self.env_file.trim())
        }
    }

    /// Resolve the API key, discarding where it came from.
    ///
    /// May block: resolving a 1Password reference runs the `op` CLI, which
    /// can prompt for biometrics. Call it off the UI thread.
    pub fn resolve_api_key(&self) -> Option<String> {
        self.api_key_with_source().map(|(key, _)| key)
    }

    /// Resolve the API key: the environment first, then the env file, then
    /// the value kept in the config itself. The second element describes the
    /// source, for `--check` to report without printing the key.
    pub fn api_key_with_source(&self) -> Option<(String, String)> {
        for name in API_KEY_ENV {
            if let Ok(key) = std::env::var(name) {
                let key = key.trim().to_string();
                if !key.is_empty() {
                    return Some((key, format!("${name}")));
                }
            }
        }

        let path = self.env_file_path();
        if path.exists() {
            match secret::read_key(&path) {
                Ok(resolved) => return Some(resolved),
                Err(error) => tracing::warn!("{error:#}"),
            }
        }

        let key = self.api_key.trim();
        (!key.is_empty()).then(|| (key.to_string(), "applet settings".to_string()))
    }

    /// `{api_base}/audio/transcriptions`, tolerating a trailing slash.
    pub fn transcription_url(&self) -> String {
        format!(
            "{}/audio/transcriptions",
            self.api_base.trim_end_matches('/')
        )
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}
