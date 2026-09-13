// SPDX-License-Identifier: MPL-2.0

//! Speech-to-text against an OpenAI-compatible `/audio/transcriptions`
//! endpoint. The same request shape works for OpenAI, Groq, and the HTTP
//! servers shipped with whisper.cpp and faster-whisper, so the endpoint is
//! configuration rather than code.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::multipart::{Form, Part};

use crate::secret::ApiKey;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Everything the request needs, resolved from config up front so this
/// module never touches `cosmic-config`.
#[derive(Clone, Debug)]
pub struct Request {
    pub url: String,
    /// `None` for local servers that do not authenticate, and for endpoints
    /// not trusted with it — see `WhisprConfig::endpoint_may_carry_key`.
    pub api_key: Option<ApiKey>,
    pub model: String,
    /// ISO-639-1 code, or `None` to let the service detect the language.
    pub language: Option<String>,
    /// Optional biasing prompt.
    pub prompt: Option<String>,
}

pub async fn transcribe(request: Request, wav: Vec<u8>) -> Result<String> {
    let bytes = wav.len();
    let part = Part::bytes(wav)
        .file_name("speech.wav")
        .mime_str("audio/wav")
        .context("cannot build the audio upload")?;

    let mut form = Form::new()
        .part("file", part)
        .text("model", request.model.clone())
        .text("response_format", "json")
        // Deterministic output: dictation should not be creative.
        .text("temperature", "0");

    if let Some(language) = request.language.filter(|value| !value.is_empty()) {
        form = form.text("language", language);
    }
    if let Some(prompt) = request.prompt.filter(|value| !value.is_empty()) {
        form = form.text("prompt", prompt);
    }

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("cannot build the HTTP client")?;

    let mut builder = client.post(&request.url).multipart(form);
    if let Some(key) = request.api_key.as_ref().filter(|key| !key.is_empty()) {
        builder = builder.bearer_auth(key.expose());
    }

    tracing::debug!(url = %request.url, bytes, "posting audio");
    let response = builder
        .send()
        .await
        .with_context(|| format!("cannot reach {}", request.url))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("cannot read the transcription response")?;

    if !status.is_success() {
        return Err(anyhow!(
            "{} — {}",
            status,
            redact(describe_error(&body), request.api_key.as_ref())
        ));
    }

    parse_transcript(&body)
}

/// Accept both `{"text": "..."}` and the bare text some servers return when
/// they ignore `response_format`.
fn parse_transcript(body: &str) -> Result<String> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(text) = value.get("text").and_then(serde_json::Value::as_str) {
            return Ok(text.trim().to_string());
        }
        return Err(anyhow!("response had no `text` field: {}", truncate(body)));
    }

    let text = body.trim();
    if text.is_empty() {
        return Err(anyhow!("the service returned an empty transcript"));
    }
    Ok(text.to_string())
}

/// Pull `error.message` out of an error body when it is there, so the popup
/// shows "Incorrect API key provided" rather than a wall of JSON.
pub(crate) fn describe_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .or_else(|| value.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| truncate(body))
}

/// Scrub credentials out of anything the endpoint sent back.
///
/// Error bodies are shown in the popup and written to the log, and the
/// endpoint controls them end to end. An authenticating proxy that echoes the
/// request headers into its 4xx body — a common enough shape — would
/// otherwise put the bearer token it just received onto the screen.
///
/// Two passes, because one is not enough: the exact key catches a verbatim
/// echo, and the generic `sk-` scan catches a truncated or reformatted one,
/// along with any other key the endpoint happens to mention.
pub(crate) fn redact(message: String, key: Option<&ApiKey>) -> String {
    let message = match key.filter(|key| !key.is_empty()) {
        Some(key) => message.replace(key.expose(), REDACTED),
        None => message,
    };
    redact_key_shaped_runs(&message)
}

const REDACTED: &str = "[redacted]";
/// Shortest run after `sk-` that is worth treating as a key rather than as
/// prose that happens to start that way.
const KEY_TAIL: usize = 16;

fn redact_key_shaped_runs(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;

    while let Some(start) = rest.find("sk-") {
        let (before, from_marker) = rest.split_at(start);
        let tail = from_marker[3..]
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .unwrap_or(from_marker.len() - 3);

        out.push_str(before);
        if tail >= KEY_TAIL {
            out.push_str(REDACTED);
        } else {
            out.push_str(&from_marker[..3 + tail]);
        }
        rest = &from_marker[3 + tail..];
    }

    out.push_str(rest);
    out
}

fn truncate(body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        return "empty response".to_string();
    }
    match body.char_indices().nth(200) {
        Some((index, _)) => format!("{}…", &body[..index]),
        None => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_transcript() {
        let text = parse_transcript(r#"{"text":"  hello world  "}"#).expect("parses");
        assert_eq!(text, "hello world");
    }

    #[test]
    fn parses_plain_text_transcript() {
        assert_eq!(
            parse_transcript("hello world\n").expect("parses"),
            "hello world"
        );
    }

    #[test]
    fn rejects_empty_transcript() {
        assert!(parse_transcript("   ").is_err());
    }

    #[test]
    fn rejects_json_without_text() {
        let error = parse_transcript(r#"{"task":"transcribe"}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no `text` field"), "{error}");
    }

    #[test]
    fn extracts_api_error_message() {
        let body =
            r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#;
        assert_eq!(describe_error(body), "Incorrect API key provided");
    }

    #[test]
    fn falls_back_to_raw_body_for_unknown_errors() {
        assert_eq!(describe_error("<html>502</html>"), "<html>502</html>");
    }

    #[test]
    fn an_echoed_key_is_redacted() {
        let key = ApiKey::new("sk-proj-abcdefghijklmnopqrstuvwxyz0123456789");
        let echoed = format!("bad credentials: Bearer {}", key.expose());
        let clean = redact(echoed, Some(&key));
        assert!(!clean.contains("sk-proj"), "{clean}");
        assert!(clean.contains("[redacted]"), "{clean}");
    }

    #[test]
    fn a_key_shaped_run_is_redacted_even_when_it_is_not_ours() {
        // A proxy that echoes a truncated or entirely different key.
        let clean = redact(
            "rejected token sk-svcacct-AAAAAAAAAAAAAAAAAAAA in header".to_string(),
            None,
        );
        assert_eq!(clean, "rejected token [redacted] in header");
    }

    #[test]
    fn prose_that_merely_starts_with_sk_survives() {
        for message in ["sk-", "the sk-1 field", "sk-short is fine"] {
            assert_eq!(redact(message.to_string(), None), message);
        }
    }

    /// The shape OpenAI actually returns on a bad key: it masks its own echo,
    /// and the masked form is a useful diagnostic rather than a secret — it
    /// says *which* key was rejected. Redacting it would cost information and
    /// protect nothing, so the scanner stops at the asterisks.
    #[test]
    fn a_masked_echo_survives_because_it_is_not_a_secret() {
        let body = r#"{"error":{"message":"Incorrect API key provided: sk-proj-****************************ghij. You can find your API key at https://platform.openai.com/account/api-keys.","type":"invalid_request_error"}}"#;
        let clean = redact(describe_error(body), None);
        assert!(clean.starts_with("Incorrect API key provided: sk-proj-***"), "{clean}");
        assert!(!clean.contains("[redacted]"), "{clean}");
    }

    /// But the same message carrying the real key is scrubbed, which is the
    /// case that matters: a proxy echoing the Authorization header verbatim.
    #[test]
    fn the_same_message_with_a_live_key_is_scrubbed() {
        let key = ApiKey::new("sk-proj-bogustestkey1234567890abcdefghij");
        let body = format!(
            r#"{{"error":{{"message":"Incorrect API key provided: {}."}}}}"#,
            key.expose()
        );
        let clean = redact(describe_error(&body), Some(&key));
        assert_eq!(clean, "Incorrect API key provided: [redacted].");
    }

    #[test]
    fn several_keys_in_one_message_all_go() {
        let clean = redact(
            "tried sk-aaaaaaaaaaaaaaaaaaaa then sk-bbbbbbbbbbbbbbbbbbbb".to_string(),
            None,
        );
        assert_eq!(clean, "tried [redacted] then [redacted]");
    }
}
