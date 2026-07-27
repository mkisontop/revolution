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

/// One streamed tool call, assembled from OpenAI-format deltas.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Accumulates `delta.tool_calls` fragments across SSE messages
/// (OpenAI-compatible wire format; id/name arrive once, arguments stream).
#[derive(Default)]
pub struct ToolCallAccum {
    calls: Vec<ToolCall>,
}

impl ToolCallAccum {
    pub fn push(&mut self, v: &serde_json::Value) {
        let Some(arr) = v
            .pointer("/choices/0/delta/tool_calls")
            .and_then(serde_json::Value::as_array)
        else {
            return;
        };
        for tc in arr {
            let idx = tc.get("index").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
            while self.calls.len() <= idx {
                self.calls.push(ToolCall::default());
            }
            let slot = &mut self.calls[idx];
            if let Some(id) = tc.get("id").and_then(serde_json::Value::as_str) {
                slot.id.push_str(id);
            }
            if let Some(n) = tc.pointer("/function/name").and_then(serde_json::Value::as_str) {
                slot.name.push_str(n);
            }
            if let Some(a) = tc
                .pointer("/function/arguments")
                .and_then(serde_json::Value::as_str)
            {
                slot.arguments.push_str(a);
            }
        }
    }

    pub fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .filter(|c| !c.name.is_empty())
            .collect()
    }
}

/// The searching side of tool-search: one non-streaming request to the same
/// model with OpenRouter's `:online` suffix, which runs their web plugin.
/// Only invoked when the brain explicitly asked to search.
async fn online_lookup(
    client: &reqwest::Client,
    cfg: &ProviderConfig,
    base_url: &str,
    query: &str,
) -> anyhow::Result<String> {
    let body = serde_json::json!({
        "model": format!("{}:online", cfg.model),
        "max_tokens": 600,
        "messages": [{
            "role": "user",
            "content": format!(
                "Search the web and answer concisely (3-6 sentences) with \
                 concrete, current facts — names, dates, numbers, patch \
                 versions: {query}"
            )
        }]
    });
    let resp = client
        .post(format!("{}/chat/completions", base_url.trim_end_matches('/')))
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "online lookup returned {status}: {}",
            body.chars().take(300).collect::<String>()
        );
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(v.pointer("/choices/0/message/content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string())
}

const WEB_SEARCH_TOOL_JSON: &str = r#"[{
    "type": "function",
    "function": {
        "name": "web_search",
        "description": "Search the live web for current facts: game patches, wiki data, release dates, prices, esports results. Call this whenever you are not fully certain of a factual answer instead of guessing.",
        "parameters": {
            "type": "object",
            "properties": {"query": {"type": "string", "description": "The search query"}},
            "required": ["query"]
        }
    }
}]"#;

async fn stream_once(
    client: reqwest::Client,
    cfg: ProviderConfig,
    req: ChatRequest,
    loop_tx: &Sender<LoopMsg>,
    tts_tx: &Sender<TtsCmd>,
) -> anyhow::Result<()> {
    let mut spec = provider_for(cfg.kind).build_request(&cfg, &req);

    // OpenRouter has no native-search passthrough; give the model an explicit
    // web_search function instead (searches only when the model decides to).
    let tool_search = cfg.enable_web_search && spec.url.contains("openrouter.ai");
    if tool_search {
        if let Some(obj) = spec.body.as_object_mut() {
            obj.remove("web_search_options"); // adapter's non-OpenRouter shape
        }
        spec.body["tools"] = serde_json::from_str(WEB_SEARCH_TOOL_JSON).expect("static tool json");
        spec.body["tool_choice"] = serde_json::json!("auto");
    }

    let mut chunker = SentenceChunker::new();
    let mut full = String::new();
    let mut first_token = false;
    let mut usage: Option<Usage> = None;
    let mut stop_reason: Option<String> = None;

    // Round 0 may end in a tool call; round 1 streams the final answer.
    for round in 0..2 {
        // Fresh parser per request (adapters keep per-response state).
        let mut provider = provider_for(cfg.kind);
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
        let mut stream = resp.bytes_stream();
        let mut accum = ToolCallAccum::default();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            for msg in sse.push(&String::from_utf8_lossy(&chunk)) {
                if msg.data.trim() != "[DONE]" {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg.data) {
                        accum.push(&v);
                    }
                }
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

        let calls = accum.finish();
        let Some(call) = calls.first().filter(|_| round == 0 && tool_search) else {
            break;
        };
        // The brain wants to look something up.
        let query = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|a| a.get("query").and_then(|q| q.as_str().map(String::from)))
            .unwrap_or_else(|| call.arguments.clone());
        let _ = loop_tx.send(LoopMsg::ToolActivity(format!("web_search: {query}")));
        let _ = tts_tx.send(TtsCmd::Sentence("Let me double-check that real quick.".into()));

        let base_url = spec.url.trim_end_matches("/chat/completions").to_string();
        let result = online_lookup(&client, &cfg, &base_url, &query)
            .await
            .unwrap_or_else(|e| format!("Search failed ({e:#}) — answer from what you know and say you could not verify."));

        // Round 2 is a CLEAN request: results injected as a system note and
        // the tool undeclared. (Formal tool-result messages + tool_choice
        // "none" both failed live — OpenRouter's Gemini translation kept
        // emitting more tool calls. No declared tool → it must answer.)
        if let Some(messages) = spec.body["messages"].as_array_mut() {
            messages.push(serde_json::json!({
                "role": "system",
                "content": format!(
                    "Web search results for \"{query}\":\n{result}\n\nAnswer \
                     the user's question now using these results. Do not \
                     mention the search mechanics."
                )
            }));
        }
        if let Some(obj) = spec.body.as_object_mut() {
            obj.remove("tools");
            obj.remove("tool_choice");
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
    fn tool_call_accumulates_across_deltas() {
        let mut a = ToolCallAccum::default();
        for frag in [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"web_search","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"query\":\"palworld "}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"latest patch\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ] {
            a.push(&serde_json::from_str(frag).unwrap());
        }
        let calls = a.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].arguments, "{\"query\":\"palworld latest patch\"}");
    }

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
