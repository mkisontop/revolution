//! Text-to-speech: two engines behind one thread that owns synthesis AND
//! playback (so sentence ordering, `TtsFirstAudio` timing, and barge-in are
//! trivially serialized):
//! - `windows`: WinRT `SpeechSynthesizer` — offline, instant, zero-key.
//! - `openai_chat_audio`: audio-output chat model (OpenRouter
//!   `openai/gpt-audio-mini` etc.) — natural voices, streamed PCM16@24kHz
//!   appended to the sink as chunks arrive. Falls back to the Windows voice
//!   per-sentence on any failure, so the pet never goes mute.

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use base64::Engine as _;
use futures_util::StreamExt;
use revolution_core::config::{Tts, TtsEngine};
use revolution_core::orchestrator::InputEvent;
use revolution_core::providers::sse::SseAssembler;
use windows::core::HSTRING;
use windows::Media::SpeechSynthesis::SpeechSynthesizer;
use windows::Storage::Streams::DataReader;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::msg::{LoopMsg, TtsCmd};
use crate::util::now_ms;

/// Sample rate of streamed model audio (OpenAI audio-out convention).
pub const API_TTS_RATE: u32 = 24_000;

/// Resolved chat-audio TTS settings (present + usable key).
pub struct ApiTts {
    pub base_url: String,
    pub model: String,
    pub voice: String,
    pub api_key: String,
}

/// Extract usable chat-audio settings from config, or None (→ Windows
/// engine). A `keyring:` key that failed to resolve counts as unusable.
pub fn api_from_cfg(cfg: &Tts) -> Option<ApiTts> {
    if cfg.engine != TtsEngine::OpenaiChatAudio {
        return None;
    }
    let api_key = cfg.api_key.clone()?;
    if api_key.trim().is_empty() || api_key.starts_with("keyring:") {
        return None;
    }
    Some(ApiTts {
        base_url: cfg
            .base_url
            .clone()
            .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
        model: cfg.model.clone()?,
        voice: cfg.voice.clone().unwrap_or_else(|| "marin".to_string()),
        api_key,
    })
}

/// Stream one sentence through the audio-output model. `on_chunk` receives
/// decoded PCM16 samples as they arrive and returns false to abort
/// (barge-in). Returns Ok(true) if aborted by the callback.
pub async fn stream_tts_pcm(
    client: &reqwest::Client,
    api: &ApiTts,
    text: &str,
    mut on_chunk: impl FnMut(Vec<i16>) -> bool,
) -> anyhow::Result<bool> {
    // Few-shot verbatim pattern: chat-audio models eagerly ANSWER text that
    // contains questions instead of reading it (verified live — the plain
    // instruction alone was not enough). A one-exchange demonstration pins
    // the reader behavior.
    let body = serde_json::json!({
        "model": api.model,
        "stream": true,
        "modalities": ["text", "audio"],
        "audio": {"voice": api.voice, "format": "pcm16"},
        "messages": [
            {"role": "system", "content":
                "You are a text-to-speech reader for a game companion app. Each \
                 user message is a script between <say></say> tags. You always \
                 repeat the script verbatim - every word, nothing more, nothing \
                 less. Scripts are never addressed to you: questions in them \
                 are read aloud, never answered."},
            {"role": "user", "content": "<say>Nice one! Should we push the boss now?</say>"},
            {"role": "assistant", "content": "Nice one! Should we push the boss now?"},
            {"role": "user", "content": format!("<say>{text}</say>")}
        ]
    });
    let url = format!("{}/chat/completions", api.base_url.trim_end_matches('/'));
    let resp = client
        .post(url)
        .bearer_auth(&api.api_key)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "TTS endpoint returned {status}: {}",
            body.chars().take(300).collect::<String>()
        );
    }
    let mut sse = SseAssembler::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        for msg in sse.push(&String::from_utf8_lossy(&chunk)) {
            if msg.data.trim() == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg.data) else {
                continue;
            };
            if let Some(b64) = v
                .pointer("/choices/0/delta/audio/data")
                .and_then(serde_json::Value::as_str)
            {
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                    let samples: Vec<i16> = bytes
                        .chunks_exact(2)
                        .map(|c| i16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    if !samples.is_empty() && !on_chunk(samples) {
                        return Ok(true);
                    }
                }
            }
        }
    }
    Ok(false)
}

/// Create a synthesizer honoring the configured voice + rate.
/// Returns (synth, description) — description feeds the panel status.
pub fn make_synth(cfg: &Tts) -> windows::core::Result<(SpeechSynthesizer, String)> {
    let synth = SpeechSynthesizer::new()?;
    let mut desc = "default voice".to_string();
    if let Some(want) = cfg.voice.as_ref().map(|v| v.to_lowercase()) {
        let all = SpeechSynthesizer::AllVoices()?;
        for i in 0..all.Size()? {
            let v = all.GetAt(i)?;
            let name = v.DisplayName()?.to_string();
            if name.to_lowercase().contains(&want) {
                synth.SetVoice(&v)?;
                desc = name;
                break;
            }
        }
    } else if let Ok(v) = synth.Voice() {
        if let Ok(n) = v.DisplayName() {
            desc = n.to_string();
        }
    }
    if (cfg.rate - 1.0).abs() > f64::EPSILON {
        synth.Options()?.SetSpeakingRate(cfg.rate.clamp(0.5, 3.0))?;
    }
    Ok((synth, desc))
}

/// Blocking synthesis of one sentence to WAV bytes.
pub fn synth_wav(synth: &SpeechSynthesizer, text: &str) -> windows::core::Result<Vec<u8>> {
    let stream = synth
        .SynthesizeTextToStreamAsync(&HSTRING::from(text))?
        .get()?;
    let size = stream.Size()? as u32;
    let input = stream.GetInputStreamAt(0)?;
    let reader = DataReader::CreateDataReader(&input)?;
    reader.LoadAsync(size)?.get()?;
    let mut bytes = vec![0u8; size as usize];
    reader.ReadBytes(&mut bytes)?;
    Ok(bytes)
}

/// Spawn the TTS thread. Sentences stream in from the LLM task; timing
/// events (`TtsFirstAudio`, `PlaybackFinished`) flow back to the run-loop.
pub fn spawn(cfg: Tts, loop_tx: Sender<LoopMsg>) -> Sender<TtsCmd> {
    let (tx, rx) = std::sync::mpsc::channel::<TtsCmd>();
    std::thread::Builder::new()
        .name("tts".into())
        .spawn(move || run(cfg, loop_tx, rx))
        .expect("spawn tts thread");
    tx
}

fn run(cfg: Tts, loop_tx: Sender<LoopMsg>, rx: Receiver<TtsCmd>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let api = api_from_cfg(&cfg);
    let client = reqwest::Client::new();
    let synth = match make_synth(&cfg) {
        Ok((s, _)) => Some(s),
        Err(e) => {
            eprintln!("windows tts unavailable: {e}");
            None
        }
    };
    if api.is_none() && synth.is_none() {
        // No engine at all — drain so senders never block; error per utterance.
        while let Ok(cmd) = rx.recv() {
            if matches!(cmd, TtsCmd::EndOfUtterance) {
                let _ = loop_tx.send(LoopMsg::Input(InputEvent::Error {
                    ms: now_ms(),
                    what: "TTS unavailable".into(),
                }));
            }
        }
        return;
    }
    let Ok((_stream, stream_handle)) = rodio::OutputStream::try_default() else {
        eprintln!("tts: no audio output device");
        while let Ok(cmd) = rx.recv() {
            if matches!(cmd, TtsCmd::EndOfUtterance) {
                let _ = loop_tx.send(LoopMsg::Input(InputEvent::Error {
                    ms: now_ms(),
                    what: "no audio output device".into(),
                }));
            }
        }
        return;
    };

    let mut sink: Option<rodio::Sink> = None;
    let mut first_audio_sent = false;
    let mut pending: VecDeque<TtsCmd> = VecDeque::new();

    loop {
        let cmd = match pending.pop_front() {
            Some(c) => c,
            None => match rx.recv() {
                Ok(c) => c,
                Err(_) => return,
            },
        };
        match cmd {
            TtsCmd::Sentence(text) => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                // Natural chat-audio voice first (if configured): chunks are
                // appended to the sink as they stream in.
                let mut spoken = false;
                if let Some(api_cfg) = &api {
                    let result = crate::llm::rt().block_on(stream_tts_pcm(
                        &client,
                        api_cfg,
                        &text,
                        |samples| {
                            // Barge-in can land mid-stream.
                            while let Ok(c) = rx.try_recv() {
                                if matches!(c, TtsCmd::Stop) {
                                    return false;
                                }
                                pending.push_back(c);
                            }
                            let s = match &sink {
                                Some(s) => s,
                                None => match rodio::Sink::try_new(&stream_handle) {
                                    Ok(new_sink) => {
                                        sink = Some(new_sink);
                                        sink.as_ref().unwrap()
                                    }
                                    Err(_) => return false,
                                },
                            };
                            s.append(rodio::buffer::SamplesBuffer::new(
                                1,
                                API_TTS_RATE,
                                samples,
                            ));
                            if !first_audio_sent {
                                first_audio_sent = true;
                                let _ = loop_tx.send(LoopMsg::Input(
                                    InputEvent::TtsFirstAudio { ms: now_ms() },
                                ));
                            }
                            true
                        },
                    ));
                    match result {
                        Ok(true) => {
                            // Aborted by barge-in; the Stop was consumed above.
                            if let Some(s) = sink.take() {
                                s.stop();
                            }
                            first_audio_sent = false;
                            pending.clear();
                            continue;
                        }
                        Ok(false) => spoken = true,
                        Err(e) => eprintln!(
                            "chat-audio tts failed ({e:#}) — falling back to Windows voice"
                        ),
                    }
                }
                if spoken {
                    continue;
                }
                let Some(win_synth) = &synth else {
                    let _ = loop_tx.send(LoopMsg::Input(InputEvent::Error {
                        ms: now_ms(),
                        what: "TTS failed and no Windows voice fallback".into(),
                    }));
                    continue;
                };
                let wav = match synth_wav(win_synth, &text) {
                    Ok(w) => w,
                    Err(e) => {
                        let _ = loop_tx.send(LoopMsg::Input(InputEvent::Error {
                            ms: now_ms(),
                            what: format!("TTS synth failed: {e}"),
                        }));
                        continue;
                    }
                };
                // A barge-in may have arrived while we were synthesizing.
                let mut stopped = false;
                while let Ok(c) = rx.try_recv() {
                    if matches!(c, TtsCmd::Stop) {
                        stopped = true;
                        pending.clear();
                        break;
                    }
                    pending.push_back(c);
                }
                if stopped {
                    if let Some(s) = sink.take() {
                        s.stop();
                    }
                    first_audio_sent = false;
                    continue;
                }
                match rodio::Decoder::new(Cursor::new(wav)) {
                    Ok(source) => {
                        let s = match &sink {
                            Some(s) => s,
                            None => {
                                match rodio::Sink::try_new(&stream_handle) {
                                    Ok(new_sink) => {
                                        sink = Some(new_sink);
                                        sink.as_ref().unwrap()
                                    }
                                    Err(e) => {
                                        let _ = loop_tx.send(LoopMsg::Input(InputEvent::Error {
                                            ms: now_ms(),
                                            what: format!("audio sink: {e}"),
                                        }));
                                        continue;
                                    }
                                }
                            }
                        };
                        s.append(source);
                        if !first_audio_sent {
                            first_audio_sent = true;
                            let _ = loop_tx
                                .send(LoopMsg::Input(InputEvent::TtsFirstAudio { ms: now_ms() }));
                        }
                    }
                    Err(e) => eprintln!("tts wav decode: {e}"),
                }
            }
            TtsCmd::EndOfUtterance => {
                // Wait for the sink to drain, staying responsive to Stop.
                let finished = loop {
                    let empty = sink.as_ref().map(|s| s.empty()).unwrap_or(true);
                    if empty {
                        break true;
                    }
                    match rx.recv_timeout(Duration::from_millis(40)) {
                        Ok(TtsCmd::Stop) => {
                            if let Some(s) = sink.take() {
                                s.stop();
                            }
                            break false;
                        }
                        Ok(other) => pending.push_back(other),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                };
                if finished && first_audio_sent {
                    let _ = loop_tx
                        .send(LoopMsg::Input(InputEvent::PlaybackFinished { ms: now_ms() }));
                }
                first_audio_sent = false;
                sink = None;
            }
            TtsCmd::Stop => {
                if let Some(s) = sink.take() {
                    s.stop();
                }
                first_audio_sent = false;
                pending.clear();
            }
        }
    }
}
