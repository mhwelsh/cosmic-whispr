// SPDX-License-Identifier: MPL-2.0

//! Persisted settings, stored through `cosmic-config` so COSMIC Settings and
//! the applet see the same values.

use crate::secret;
use cosmic::cosmic_config::cosmic_config_derive::CosmicConfigEntry;
use cosmic::cosmic_config::{self, Config, CosmicConfigEntry};
use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "dev.mhwelsh.CosmicWhispr";
pub const CONFIG_VERSION: u64 = 1;

/// Environment variables checked ahead of the keyring, as a debugging
/// override — `COSMIC_WHISPR_API_KEY=sk-... cosmic-whispr` runs against a
/// throwaway key without disturbing what is stored.
pub const API_KEY_ENV: [&str; 2] = ["COSMIC_WHISPR_API_KEY", "OPENAI_API_KEY"];

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
    /// The `op://vault/item/field` reference the key was last imported
    /// from, remembered so re-importing after a rotation is one click.
    ///
    /// This is a reference, not a secret, which is why it can live in the
    /// plain config file. Nothing reads it to answer "what is the API key" —
    /// that question is only ever put to the keyring.
    ///
    /// The default points at the author's item. It resolves for nobody else,
    /// since `op` reads whichever account is signed in, so the cost to anyone
    /// else is one field to overwrite.
    pub op_reference: String,
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
    /// Run the transcript through a small chat model to strip filler words,
    /// stutters, and false starts. Costs a second round trip (~1 s).
    pub cleanup: bool,
    /// Model used for that second pass. It must be cheap, fast, and hard to
    /// talk out of its instructions — see `cleanup.rs`.
    pub cleanup_model: String,
}

impl Default for WhisprConfig {
    fn default() -> Self {
        Self {
            api_base: "https://api.openai.com/v1".into(),
            model: "whisper-1".into(),
            language: String::new(),
            prompt: String::new(),
            op_reference: "op://Private/openai-api/credential".into(),
            input_device: String::new(),
            type_delay_ms: 4,
            trailing_space: true,
            max_seconds: 300,
            cleanup: true,
            cleanup_model: "gpt-5.4-nano".into(),
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

    /// Move a key left behind by an older version, which kept it in plain
    /// text in the config file, into the keyring and blank the old entry.
    ///
    /// cosmic-config has no "remove", so blanking is the erase. Failures are
    /// logged and otherwise ignored: the worst case is pasting the key again.
    pub fn migrate_plaintext_key(handle: &Config) {
        use cosmic_config::{ConfigGet, ConfigSet};

        let Ok(key) = handle.get::<String>("api_key") else {
            return;
        };
        if key.trim().is_empty() {
            return;
        }

        match secret::store(&key) {
            Ok(()) => {
                if let Err(error) = handle.set("api_key", String::new()) {
                    tracing::warn!(%error, "moved the API key to the keyring but could not \
                                            clear the old plain-text copy");
                } else {
                    tracing::info!("moved the API key out of the config file into the keyring");
                }
            }
            Err(error) => tracing::warn!("{error:#}"),
        }
    }

    /// Resolve the API key, discarding where it came from.
    ///
    /// May block: reading the keyring talks to the Secret Service over
    /// D-Bus, which can raise an unlock prompt. Call it off the UI thread.
    pub fn resolve_api_key(&self) -> Option<String> {
        self.api_key_with_source().map(|(key, _)| key)
    }

    /// Resolve the API key: the environment override first, then the
    /// keyring. The second element describes the source, for `--check` to
    /// report without printing the key.
    pub fn api_key_with_source(&self) -> Option<(String, String)> {
        for name in API_KEY_ENV {
            if let Ok(key) = std::env::var(name) {
                let key = key.trim().to_string();
                if !key.is_empty() {
                    return Some((key, format!("${name}")));
                }
            }
        }

        match secret::load() {
            Ok(Some(key)) => Some((key, "the keyring".to_string())),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!("{error:#}");
                None
            }
        }
    }

    /// `{api_base}/audio/transcriptions`, tolerating a trailing slash.
    pub fn transcription_url(&self) -> String {
        format!(
            "{}/audio/transcriptions",
            self.api_base.trim_end_matches('/')
        )
    }

    /// `{api_base}/chat/completions`, for the cleanup pass.
    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.api_base.trim_end_matches('/'))
    }
}
