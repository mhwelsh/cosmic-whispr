// SPDX-License-Identifier: MPL-2.0

//! Speech-to-text against an OpenAI-compatible `/audio/transcriptions`
//! endpoint. The same request shape works for OpenAI, Groq, and the HTTP
//! servers shipped with whisper.cpp and faster-whisper, so the endpoint is
//! configuration rather than code.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::multipart::{Form, Part};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Everything the request needs, resolved from config up front so this
/// module never touches `cosmic-config`.
#[derive(Clone, Debug)]
pub struct Request {
    pub url: String,
    /// `None` for local servers that do not authenticate.
    pub api_key: Option<String>,
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
    if let Some(key) = request.api_key.filter(|key| !key.is_empty()) {
        builder = builder.bearer_auth(key);
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
        return Err(anyhow!("{} — {}", status, describe_error(&body)));
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
fn describe_error(body: &str) -> String {
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
}
