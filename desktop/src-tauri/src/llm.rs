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

/// Strip markdown so the voice doesn't read decoration out loud. GPT-5.x
/// models bold aggressively (`**Palworld**`) even when told not to.
pub fn speakable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // Emphasis/code markers carry no meaning aloud.
            '*' | '`' | '_' | '~' => {}
            // Headings/quote markers only at the start of a line.
            '#' | '>' if out.is_empty() || out.ends_with('\n') => {
                while matches!(chars.peek(), Some('#' | '>' | ' ')) {
                    chars.next();
                }
            }
            // Bullets read as pauses, not "dash".
            '-' if out.is_empty() || out.ends_with('\n') => {
                while matches!(chars.peek(), Some(' ')) {
                    chars.next();
                }
            }
            _ => out.push(c),
        }
    }
    out
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
    search_cfg: Option<ProviderConfig>,
    req: ChatRequest,
    loop_tx: Sender<LoopMsg>,
    tts_tx: Sender<TtsCmd>,
) -> tokio::task::JoinHandle<()> {
    rt().spawn(async move {
        if let Err(e) = stream_once(client, cfg, search_cfg, req, &loop_tx, &tts_tx).await {
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

/// Pull one string field out of a tool call's (possibly malformed) JSON args.
fn arg_str(arguments: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get(key)?
        .as_str()
        .map(String::from)
}

/// Execute a request through the provider adapter and collect the streamed
/// text into one string (no TTS, no tools). The workhorse behind the
/// summarizer, the ambient lane, and the panel's "test brain" button.
pub async fn collect_stream(
    client: &reqwest::Client,
    cfg: &ProviderConfig,
    req: &ChatRequest,
) -> anyhow::Result<String> {
    let spec = provider_for(cfg.kind).build_request(cfg, req);
    let mut provider = provider_for(cfg.kind);
    let mut rb = match spec.method {
        "GET" => client.get(&spec.url),
        _ => client.post(&spec.url),
    };
    for (k, v) in &spec.headers {
        rb = rb.header(k, v);
    }
    let resp = rb
        .json(&spec.body)
        .timeout(std::time::Duration::from_secs(45))
        .send()
        .await?;
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
    let mut full = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        for msg in sse.push(&String::from_utf8_lossy(&chunk)) {
            for ev in provider.parse_sse(&msg) {
                if let StreamEvent::TextDelta(t) = ev {
                    full.push_str(&t);
                }
            }
        }
    }
    Ok(full.trim().to_string())
}

/// What a background summary is for — decides the completion message.
pub enum SummarizeTarget {
    Episode { game_id: i64, started_epoch_ms: u64 },
    /// Carries the heuristic fallback used if the model call fails, so
    /// compaction always completes.
    Compaction { fallback: String },
}

fn summarize_request(transcript_text: &str) -> ChatRequest {
    ChatRequest {
        system: revolution_core::prompt::summarizer_instruction(),
        context_block: String::new(),
        turns: vec![revolution_core::providers::Turn::User {
            text: transcript_text.to_string(),
            images: Vec::new(),
        }],
    }
}

/// Model config tuned for summaries: short output, never the slow dial.
fn summarizer_cfg(mut cfg: ProviderConfig) -> ProviderConfig {
    cfg.max_output_tokens = 300;
    cfg.enable_web_search = false;
    cfg
}

/// Fire-and-forget background summary (session episode or compaction).
pub fn spawn_summarize(
    client: reqwest::Client,
    cfg: ProviderConfig,
    transcript_text: String,
    target: SummarizeTarget,
    loop_tx: Sender<LoopMsg>,
) {
    rt().spawn(async move {
        let cfg = summarizer_cfg(cfg);
        let req = summarize_request(&transcript_text);
        let result = collect_stream(&client, &cfg, &req).await;
        match (result, target) {
            (Ok(s), SummarizeTarget::Episode { game_id, started_epoch_ms }) if !s.is_empty() => {
                let _ = loop_tx.send(LoopMsg::EpisodeReady {
                    game_id,
                    started_epoch_ms,
                    summary: s,
                });
            }
            (Err(e), SummarizeTarget::Episode { .. }) => {
                eprintln!("episode summary failed: {e:#}");
            }
            (_, SummarizeTarget::Episode { .. }) => {}
            (Ok(s), SummarizeTarget::Compaction { fallback }) => {
                let summary = if s.is_empty() { fallback } else { s };
                let _ = loop_tx.send(LoopMsg::CompactionReady(summary));
            }
            (Err(e), SummarizeTarget::Compaction { fallback }) => {
                eprintln!("compaction summary failed ({e:#}) — using heuristic");
                let _ = loop_tx.send(LoopMsg::CompactionReady(fallback));
            }
        }
    });
}

/// Synchronous summary with a hard deadline — the quit path only.
pub fn summarize_blocking(
    client: &reqwest::Client,
    cfg: &ProviderConfig,
    transcript_text: &str,
    timeout: std::time::Duration,
) -> Option<String> {
    let cfg = summarizer_cfg(cfg.clone());
    let req = summarize_request(transcript_text);
    rt().block_on(async {
        tokio::time::timeout(timeout, collect_stream(client, &cfg, &req))
            .await
            .ok()?
            .ok()
            .filter(|s| !s.is_empty())
    })
}

/// The searching side of tool-search: one non-streaming request to a
/// web-capable model. OpenRouter models get the `:online` suffix (their web
/// plugin); anything else is called as-is, so a natively-searching endpoint
/// or a Perplexity-style model works too. Only invoked when the brain
/// explicitly asked to search.
async fn online_lookup(
    client: &reqwest::Client,
    cfg: &ProviderConfig,
    base_url: &str,
    query: &str,
) -> anyhow::Result<String> {
    let model = if base_url.contains("openrouter.ai") && !cfg.model.contains(":online") {
        format!("{}:online", cfg.model)
    } else {
        cfg.model.clone()
    };
    let body = serde_json::json!({
        "model": model,
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

const WEB_SEARCH_TOOL_JSON: &str = r#"{
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
}"#;

/// The memory tools the system core promises the brain (durable facts about
/// the player, not trivia). Executed locally by the run-loop.
const MEMORY_TOOLS_JSON: &str = r#"[{
    "type": "function",
    "function": {
        "name": "remember",
        "description": "Save one durable fact about the player for future sessions: their build, goals, preferences, running jokes. One short sentence per call. Never for game trivia or things visible on screen.",
        "parameters": {
            "type": "object",
            "properties": {"fact": {"type": "string", "description": "The fact, one sentence"}},
            "required": ["fact"]
        }
    }
}, {
    "type": "function",
    "function": {
        "name": "update_profile",
        "description": "Rewrite the player profile card for this game (the '## Player profile' block you receive). Use when the build/goals changed substantially; send the complete new markdown.",
        "parameters": {
            "type": "object",
            "properties": {"markdown": {"type": "string", "description": "The full replacement profile markdown"}},
            "required": ["markdown"]
        }
    }
}]"#;

async fn stream_once(
    client: reqwest::Client,
    cfg: ProviderConfig,
    search_cfg: Option<ProviderConfig>,
    req: ChatRequest,
    loop_tx: &Sender<LoopMsg>,
    tts_tx: &Sender<TtsCmd>,
) -> anyhow::Result<()> {
    let mut spec = provider_for(cfg.kind).build_request(&cfg, &req);

    // Endpoints without native search passthrough (OpenRouter, local routers
    // like 9router) get an explicit web_search function instead, executed by
    // the search role — so the brain searches only when it decides to.
    let searcher = search_cfg.filter(|s| !s.api_key.trim().is_empty()).or_else(|| {
        spec.url
            .contains("openrouter.ai")
            .then(|| cfg.clone())
    });
    let tool_search = cfg.enable_web_search
        && searcher.is_some()
        && matches!(cfg.kind, revolution_core::providers::ProviderKind::OpenaiCompat);
    // remember/update_profile ride the same OpenAI tool wire; they need no
    // searcher — the run-loop executes them locally.
    let tools_declared = matches!(cfg.kind, revolution_core::providers::ProviderKind::OpenaiCompat);
    if tools_declared {
        if let Some(obj) = spec.body.as_object_mut() {
            obj.remove("web_search_options"); // adapter's non-OpenRouter shape
        }
        let mut tools: Vec<serde_json::Value> =
            serde_json::from_str(MEMORY_TOOLS_JSON).expect("static tool json");
        if tool_search {
            tools.push(serde_json::from_str(WEB_SEARCH_TOOL_JSON).expect("static tool json"));
        }
        spec.body["tools"] = serde_json::json!(tools);
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
                                let spoken = speakable(&s);
                                if !spoken.trim().is_empty() {
                                    let _ = tts_tx.send(TtsCmd::Sentence(spoken));
                                }
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
        if round != 0 || !tools_declared || calls.is_empty() {
            break;
        }

        // Execute memory tools locally (fire-and-forget to the run-loop);
        // collect at most one web_search for the round trip.
        let mut memory_notes: Vec<&str> = Vec::new();
        let mut search_query: Option<String> = None;
        for call in &calls {
            match call.name.as_str() {
                "remember" => {
                    if let Some(fact) = arg_str(&call.arguments, "fact") {
                        let _ = loop_tx.send(LoopMsg::ToolActivity("remember".into()));
                        let _ = loop_tx.send(LoopMsg::ToolRemember(fact));
                        memory_notes.push("The fact was saved to memory.");
                    }
                }
                "update_profile" => {
                    if let Some(md) = arg_str(&call.arguments, "markdown") {
                        let _ = loop_tx.send(LoopMsg::ToolActivity("profile".into()));
                        let _ = loop_tx.send(LoopMsg::ToolUpdateProfile(md));
                        memory_notes.push("The profile was updated.");
                    }
                }
                "web_search" if tool_search && search_query.is_none() => {
                    search_query = Some(
                        arg_str(&call.arguments, "query")
                            .unwrap_or_else(|| call.arguments.clone()),
                    );
                }
                _ => {}
            }
        }
        if memory_notes.is_empty() && search_query.is_none() {
            break;
        }

        // Round 2 is a CLEAN request: results injected as a USER message and
        // the tools undeclared. Both details are load-bearing, each found by
        // live failure: formal tool-result messages and tool_choice "none"
        // still produce more tool calls (OpenRouter/Gemini), and a mid-
        // conversation *system* message is dropped entirely by 9router's
        // Codex translation — the model then claims it has no web access.
        let mut round2 = String::new();
        if let Some(query) = search_query {
            let _ = loop_tx.send(LoopMsg::ToolActivity(format!("web_search: {query}")));
            let _ =
                tts_tx.send(TtsCmd::Sentence("Let me double-check that real quick.".into()));
            let search_provider = searcher.as_ref().expect("tool_search implies a searcher");
            let search_base = search_provider
                .base_url
                .clone()
                .unwrap_or_else(|| spec.url.trim_end_matches("/chat/completions").to_string());
            let result = online_lookup(&client, search_provider, &search_base, &query)
                .await
                .unwrap_or_else(|e| format!("Search failed ({e:#}) — answer from what you know and say you could not verify."));
            round2.push_str(&format!("Web search results for \"{query}\":\n{result}\n\n"));
        }
        if !memory_notes.is_empty() {
            round2.push_str(&format!("[{}]\n\n", memory_notes.join(" ")));
        }
        round2.push_str(
            "Answer my question above now. Do not mention the search or memory mechanics.",
        );
        if let Some(messages) = spec.body["messages"].as_array_mut() {
            messages.push(serde_json::json!({"role": "user", "content": round2}));
        }
        if let Some(obj) = spec.body.as_object_mut() {
            obj.remove("tools");
            obj.remove("tool_choice");
        }
    }

    if let Some(rest) = chunker.flush() {
        let spoken = speakable(&rest);
        if !spoken.trim().is_empty() {
            let _ = tts_tx.send(TtsCmd::Sentence(spoken));
        }
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
    fn markdown_never_reaches_the_voice() {
        assert_eq!(
            speakable("You're in **Palworld** at your `base` — catch a _Foxparks_."),
            "You're in Palworld at your base — catch a Foxparks."
        );
        assert_eq!(speakable("## Next steps\n- grab wood\n- build it"), "Next steps\ngrab wood\nbuild it");
        // Mid-word hyphens and math survive.
        assert_eq!(speakable("It's a 2-shot combo, 15-20 damage."), "It's a 2-shot combo, 15-20 damage.");
    }

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
