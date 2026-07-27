//! Batch speech-to-text, two wire shapes behind one call:
//! - `openai_compat_batch`: multipart `/audio/transcriptions` (Groq/OpenAI
//!   Whisper).
//! - `openai_chat_audio`: an `input_audio` chat-completion part — how
//!   OpenRouter serves transcription (it has no Whisper endpoint); measured
//!   1.9 s + exact transcript on `google/gemini-2.5-flash-lite`.
//! Streaming STT (Deepgram) can replace either without touching the
//! orchestrator — it only changes who sends `SttFinal`.

use base64::Engine;
use revolution_core::config::{Stt, SttKind};

pub async fn transcribe(
    client: &reqwest::Client,
    cfg: &Stt,
    wav: Vec<u8>,
) -> anyhow::Result<String> {
    match cfg.kind {
        SttKind::OpenaiCompatBatch => whisper_batch(client, cfg, wav).await,
        SttKind::OpenaiChatAudio => chat_audio(client, cfg, wav).await,
    }
}

async fn chat_audio(client: &reqwest::Client, cfg: &Stt, wav: Vec<u8>) -> anyhow::Result<String> {
    let lang_hint = cfg
        .language
        .as_ref()
        .map(|l| format!(" The speech is in language '{l}'."))
        .unwrap_or_default();
    let body = serde_json::json!({
        "model": cfg.model,
        "temperature": 0,
        "max_tokens": 300,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": format!(
                    "Transcribe this audio exactly. Output only the transcript text, \
                     nothing else. If there is no clear speech, output nothing.{lang_hint}"
                )},
                {"type": "input_audio", "input_audio": {
                    "data": base64::engine::general_purpose::STANDARD.encode(&wav),
                    "format": "wav"
                }}
            ]
        }]
    });
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let resp = client
        .post(url)
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "STT chat endpoint returned {status}: {}",
            body.chars().take(300).collect::<String>()
        );
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(v.pointer("/choices/0/message/content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string())
}

async fn whisper_batch(
    client: &reqwest::Client,
    cfg: &Stt,
    wav: Vec<u8>,
) -> anyhow::Result<String> {
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("question.wav")
        .mime_str("audio/wav")?;
    let mut form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", cfg.model.clone())
        .text("response_format", "json")
        .text("temperature", "0");
    if let Some(lang) = &cfg.language {
        form = form.text("language", lang.clone());
    }
    let url = format!(
        "{}/audio/transcriptions",
        cfg.base_url.trim_end_matches('/')
    );
    let resp = client
        .post(url)
        .bearer_auth(&cfg.api_key)
        .multipart(form)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("STT endpoint returned {status}: {}", body.chars().take(300).collect::<String>());
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(v["text"].as_str().unwrap_or_default().trim().to_string())
}
