// SPDX-License-Identifier: MPL-2.0

//! Persisted settings, stored through `cosmic-config` so COSMIC Settings and
//! the applet see the same values.

use crate::secret::{self, ApiKey};
use cosmic::cosmic_config::cosmic_config_derive::CosmicConfigEntry;
use cosmic::cosmic_config::{self, Config, CosmicConfigEntry};
use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "dev.mhwelsh.CosmicWhispr";
pub const CONFIG_VERSION: u64 = 1;

/// Consulted only when the keyring has nothing to give — a machine with no
/// Secret Service running, which is how the applet stays usable headless and
/// how the live tests get a key in CI.
///
/// Deliberately not `OPENAI_API_KEY`: that name is exported on half the
/// developer machines in the world, and a variable meant for some other tool
/// silently answering for this one is a trap. Nothing sets this name except
/// somebody who typed it.
pub const API_KEY_ENV: &str = "COSMIC_WHISPR_API_KEY";

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

    /// Resolve the API key for the configured endpoint.
    ///
    /// Returns `None` when the endpoint has not earned it — see
    /// [`Self::endpoint_may_carry_key`] — so a mistyped scheme costs an
    /// authentication failure rather than the key itself.
    ///
    /// May block: reading the keyring talks to the Secret Service over
    /// D-Bus, which can raise an unlock prompt. Call it off the UI thread.
    pub fn resolve_api_key(&self) -> Option<ApiKey> {
        if !self.endpoint_may_carry_key() {
            tracing::warn!(
                api_base = %self.api_base,
                "endpoint is neither HTTPS nor loopback; withholding the API key"
            );
            return None;
        }
        self.api_key_with_source().map(|(key, _)| key)
    }

    /// Resolve the API key: the keyring first, and only then the
    /// environment. The second element describes the source, for `--check`
    /// to report without printing the key.
    ///
    /// The keyring wins deliberately. An environment variable that outranked
    /// it could shadow the key you deliberately stored — rotating with
    /// `--set-key-from` would report success, the keyring would genuinely
    /// hold the new key, and the applet would keep sending the old one. As a
    /// fallback it cannot do that, while still covering the case it is
    /// actually for: no Secret Service to talk to.
    pub fn api_key_with_source(&self) -> Option<(ApiKey, String)> {
        match secret::load() {
            Ok(Some(key)) => return Some((key, "the keyring".to_string())),
            Ok(None) => {}
            Err(error) => tracing::warn!("{error:#}"),
        }

        let key = std::env::var(API_KEY_ENV).ok()?;
        let key = key.trim();
        (!key.is_empty()).then(|| (ApiKey::new(key), format!("${API_KEY_ENV}")))
    }

    /// May the configured endpoint be trusted with the API key?
    ///
    /// The key is well protected at rest now, but its destination is not:
    /// `api_base` is plain text in a config file the applet re-reads on every
    /// change. Sending a bearer token over `http://` puts it on the wire in
    /// the clear, and sending it to an arbitrary host hands it over outright,
    /// so neither happens without TLS.
    ///
    /// Loopback is the exception. A local `whisper.cpp` or `faster-whisper`
    /// server is the reason plain HTTP is supported at all, there is no
    /// network to listen on, and those servers want no key anyway.
    pub fn endpoint_may_carry_key(&self) -> bool {
        endpoint_may_carry_key(&self.api_base)
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

/// TLS, or a loopback address where there is no network to eavesdrop on.
///
/// A URL that will not parse fails closed: if we cannot tell where the key
/// would go, it does not go.
pub fn endpoint_may_carry_key(api_base: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(api_base.trim()) else {
        return false;
    };

    if url.scheme() == "https" {
        return true;
    }

    // `host_str` keeps the brackets around an IPv6 literal, which
    // `IpAddr::parse` will not take.
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');

    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_endpoints_may_carry_the_key() {
        assert!(endpoint_may_carry_key("https://api.openai.com/v1"));
        assert!(endpoint_may_carry_key("  https://api.groq.com/openai/v1  "));
    }

    #[test]
    fn plain_http_to_the_internet_may_not() {
        assert!(!endpoint_may_carry_key("http://api.openai.com/v1"));
        assert!(!endpoint_may_carry_key("http://192.168.1.5:8080/v1"));
        assert!(!endpoint_may_carry_key("http://evil.example/v1"));
    }

    #[test]
    fn loopback_may_carry_it_over_plain_http() {
        assert!(endpoint_may_carry_key("http://localhost:8080/v1"));
        assert!(endpoint_may_carry_key("http://127.0.0.1:8080/v1"));
        assert!(endpoint_may_carry_key("http://[::1]:8080/v1"));
    }

    #[test]
    fn anything_unparseable_fails_closed() {
        assert!(!endpoint_may_carry_key(""));
        assert!(!endpoint_may_carry_key("api.openai.com/v1"));
        assert!(!endpoint_may_carry_key("not a url"));
    }

    /// The regression this inversion exists to prevent: a variable left over
    /// from another tool must never answer for a key that was just stored.
    #[test]
    fn the_environment_never_shadows_a_stored_key() {
        // SAFETY: the values are removed again below, and neither name is
        // read by anything else in the test binary.
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-from-some-other-tool") };
        let config = WhisprConfig::default();

        // OPENAI_API_KEY is not consulted at all, whatever the keyring holds.
        assert!(
            config
                .api_key_with_source()
                .is_none_or(|(key, _)| key.expose() != "sk-from-some-other-tool")
        );

        unsafe { std::env::remove_var("OPENAI_API_KEY") };
    }

    #[test]
    fn a_withheld_key_is_not_resolved() {
        let config = WhisprConfig {
            api_base: "http://evil.example/v1".into(),
            ..WhisprConfig::default()
        };
        assert!(!config.endpoint_may_carry_key());
        assert!(config.resolve_api_key().is_none());
    }
}
