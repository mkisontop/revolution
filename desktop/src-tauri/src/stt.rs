//! Batch speech-to-text: POST the PTT take as WAV to an OpenAI-compatible
//! `/audio/transcriptions` endpoint (Groq's free Whisper tier is the
//! documented default). Streaming STT (Deepgram) can replace this without
//! touching the orchestrator — it only changes who sends `SttFinal`.

use revolution_core::config::Stt;

pub async fn transcribe(
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
