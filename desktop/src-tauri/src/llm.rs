//! Streaming LLM transport: execute the `HttpRequestSpec` built by
//! `revolution_core::providers`, pump the SSE body through the provider's
//! parser, and fan the normalized events out to the TTS sentence chunker,
//! the run-loop, and the UI. Abortable for barge-in.

use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use futures_util::StreamExt;
use revolution_core::orchestrator::InputEvent;
use revolution_core::providers::sse::SseAssembler;
use revolution_core::providers::{provider_for, ChatRequest, ProviderConfig, StreamEvent};

use crate::msg::{LoopMsg, TtsCmd, Usage};
use crate::util::now_ms;

/// Dedicated tokio runtime for network I/O — independent of tauri's
/// internals, and available in `--smoke` mode where tauri never starts.
pub fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Split streamed text deltas into speakable sentences for the TTS queue.
/// Emission rule: sentence terminator (.!?…) or newline, with a minimum
/// length so abbreviations don't fire early; force-flush past 220 chars.
pub struct SentenceChunker {
    buf: String,
}

impl SentenceChunker {
    pub fn new() -> Self {
        Self { buf: String::new() }
    }

    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.buf.push_str(delta);
        let mut out = Vec::new();
        loop {
            let cut = self
                .buf
                .char_indices()
                .find(|(i, c)| {
                    let boundary = matches!(c, '.' | '!' | '?' | '…' | '\n');
                    boundary && *i >= 12 && {
                        // Only cut when the terminator ends a word (next char
                        // is whitespace or end-of-buffer so far).
                        self.buf[i + c.len_utf8()..]
                            .chars()
                            .next()
                            .map(|n| n.is_whitespace())
                            .unwrap_or(false)
                    }
                })
                .map(|(i, c)| i + c.len_utf8());
            match cut {
                Some(pos) => {
                    let sentence: String = self.buf.drain(..pos).collect();
                    let sentence = sentence.trim().to_string();
                    if !sentence.is_empty() {
                        out.push(sentence);
                    }
                }
                None if self.buf.len() > 220 => {
                    // Runaway clause — cut at the last space to stay speakable.
                    let pos = self.buf.rfind(' ').unwrap_or(self.buf.len());
                    let sentence: String = self.buf.drain(..pos).collect();
                    let sentence = sentence.trim().to_string();
                    if !sentence.is_empty() {
                        out.push(sentence);
                    }
                }
                None => break,
            }
        }
        out
    }

    pub fn flush(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buf);
        let rest = rest.trim().to_string();
        (!rest.is_empty()).then_some(rest)
    }
}

/// Spawn the streaming request. The returned handle aborts the transport on
/// barge-in (TTS is stopped separately by the run-loop).
pub fn spawn_stream(
    client: reqwest::Client,
    cfg: ProviderConfig,
    req: ChatRequest,
    loop_tx: Sender<LoopMsg>,
    tts_tx: Sender<TtsCmd>,
) -> tokio::task::JoinHandle<()> {
    rt().spawn(async move {
        if let Err(e) = stream_once(client, cfg, req, &loop_tx, &tts_tx).await {
            let _ = loop_tx.send(LoopMsg::LlmFailed(format!("{e:#}")));
        }
    })
}

async fn stream_once(
    client: reqwest::Client,
    cfg: ProviderConfig,
    req: ChatRequest,
    loop_tx: &Sender<LoopMsg>,
    tts_tx: &Sender<TtsCmd>,
) -> anyhow::Result<()> {
    let mut provider = provider_for(cfg.kind);
    let spec = provider.build_request(&cfg, &req);

    let mut rb = match spec.method {
        "GET" => client.get(&spec.url),
        _ => client.post(&spec.url),
    };
    for (k, v) in &spec.headers {
        rb = rb.header(k, v);
    }
    let resp = rb.json(&spec.body).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "{} returned {status}: {}",
            spec.url,
            body.chars().take(400).collect::<String>()
        );
    }

    let mut sse = SseAssembler::new();
    let mut chunker = SentenceChunker::new();
    let mut stream = resp.bytes_stream();
    let mut full = String::new();
    let mut first_token = false;
    let mut usage: Option<Usage> = None;
    let mut stop_reason: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        for msg in sse.push(&String::from_utf8_lossy(&chunk)) {
            for ev in provider.parse_sse(&msg) {
                match ev {
                    StreamEvent::TextDelta(t) => {
                        if !first_token {
                            first_token = true;
                            let _ = loop_tx
                                .send(LoopMsg::Input(InputEvent::FirstToken { ms: now_ms() }));
                        }
                        full.push_str(&t);
                        for s in chunker.push(&t) {
                            let _ = tts_tx.send(TtsCmd::Sentence(s));
                        }
                        let _ = loop_tx.send(LoopMsg::Delta(t));
                    }
                    StreamEvent::ToolActivity(what) => {
                        let _ = loop_tx.send(LoopMsg::ToolActivity(what));
                    }
                    StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                    } => {
                        usage = Some(Usage {
                            input_tokens,
                            output_tokens,
                            cache_read_tokens,
                        });
                    }
                    StreamEvent::Done { stop_reason: sr } => stop_reason = sr,
                }
            }
        }
    }

    if let Some(rest) = chunker.flush() {
        let _ = tts_tx.send(TtsCmd::Sentence(rest));
    }
    let _ = tts_tx.send(TtsCmd::EndOfUtterance);
    let _ = loop_tx.send(LoopMsg::LlmDone {
        full_text: full,
        usage,
        stop_reason,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunker_emits_on_sentence_boundaries() {
        let mut c = SentenceChunker::new();
        assert!(c.push("Digtoise is").is_empty());
        let out = c.push(" your coal fix. Park it on the ore");
        assert_eq!(out, vec!["Digtoise is your coal fix."]);
        assert!(c.push(" nodes by").is_empty());
        assert_eq!(c.flush().as_deref(), Some("Park it on the ore nodes by"));
    }

    #[test]
    fn chunker_does_not_cut_decimals() {
        let mut c = SentenceChunker::new();
        assert!(c.push("It has a 2.5 second cooldown so").is_empty());
        // Decimal point never splits; a terminator at end-of-stream waits
        // for flush (the next delta could continue the token).
        let out = c.push(" wait for it. Then dodge.");
        assert_eq!(out, vec!["It has a 2.5 second cooldown so wait for it."]);
        assert_eq!(c.flush().as_deref(), Some("Then dodge."));
    }
}
