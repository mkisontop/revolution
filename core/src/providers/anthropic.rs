//! Anthropic Claude Messages API adapter.
//!
//! Gets the Claude-specific optimizations from DESIGN.md §6.2: explicit
//! 1h-TTL `cache_control` breakpoints on the frozen system prompt and the
//! per-game context block, a moving breakpoint on the newest turn, native
//! `web_search_20260209`, and the `output_config.effort` latency knob.
//! `SystemNote` turns map to real mid-conversation `role:"system"` messages
//! (supported on current Opus-tier models; preserves the cached prefix).

use serde_json::{json, Value};

use super::sse::SseMessage;
use super::{ChatProvider, ChatRequest, HttpRequestSpec, ProviderConfig, StreamEvent, Turn};

pub const DEFAULT_BASE: &str = "https://api.anthropic.com";
pub const API_VERSION: &str = "2023-06-01";

#[derive(Default)]
pub struct Anthropic {
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    stop_reason: Option<String>,
}

impl ChatProvider for Anthropic {
    fn build_request(&self, cfg: &ProviderConfig, req: &ChatRequest) -> HttpRequestSpec {
        let base = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        let url = format!("{}/v1/messages", base.trim_end_matches('/'));

        let system = json!([
            {"type": "text", "text": req.system, "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            {"type": "text", "text": req.context_block, "cache_control": {"type": "ephemeral", "ttl": "1h"}},
        ]);

        let mut messages: Vec<Value> = Vec::new();
        for t in &req.turns {
            match t {
                Turn::User { text, images } => {
                    let mut blocks: Vec<Value> = Vec::new();
                    for im in images {
                        blocks.push(json!({
                            "type": "image",
                            "source": {"type": "base64", "media_type": im.media_type, "data": im.base64_data},
                        }));
                    }
                    blocks.push(json!({"type": "text", "text": text}));
                    messages.push(json!({"role": "user", "content": blocks}));
                }
                Turn::Assistant { text } => {
                    messages.push(json!({"role": "assistant", "content": text}));
                }
                Turn::SystemNote { text } => {
                    messages.push(json!({"role": "system", "content": text}));
                }
            }
        }
        // Moving breakpoint: cache up to the newest turn so history accrues
        // incremental cache hits (DESIGN.md §6.2).
        if let Some(last) = messages.last_mut() {
            if let Some(blocks) = last.get_mut("content").and_then(Value::as_array_mut) {
                if let Some(last_block) = blocks.last_mut() {
                    last_block["cache_control"] = json!({"type": "ephemeral"});
                }
            }
        }

        let mut body = json!({
            "model": cfg.model,
            "stream": true,
            "max_tokens": cfg.max_output_tokens,
            "system": system,
            "messages": messages,
        });
        if let Some(effort) = &cfg.effort {
            body["output_config"] = json!({"effort": effort});
        }
        if cfg.enable_web_search {
            body["tools"] = json!([
                {"type": "web_search_20260209", "name": "web_search"},
            ]);
        }

        HttpRequestSpec {
            method: "POST",
            url,
            headers: vec![
                ("x-api-key".into(), cfg.api_key.clone()),
                ("anthropic-version".into(), API_VERSION.into()),
                ("content-type".into(), "application/json".into()),
            ],
            body,
        }
    }

    fn parse_sse(&mut self, msg: &SseMessage) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<Value>(&msg.data) else {
            return out;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.input_tokens = v
                    .pointer("/message/usage/input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.cache_read_tokens = v
                    .pointer("/message/usage/cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
            }
            Some("content_block_start") => {
                if let Some(name) = v.pointer("/content_block/name").and_then(Value::as_str) {
                    out.push(StreamEvent::ToolActivity(name.to_string()));
                }
            }
            Some("content_block_delta") => {
                if let Some(t) = v.pointer("/delta/text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        out.push(StreamEvent::TextDelta(t.to_string()));
                    }
                }
            }
            Some("message_delta") => {
                if let Some(sr) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(sr.to_string());
                }
                if let Some(ot) = v.pointer("/usage/output_tokens").and_then(Value::as_u64) {
                    self.output_tokens = ot;
                }
                out.push(StreamEvent::Usage {
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                    cache_read_tokens: self.cache_read_tokens,
                });
            }
            Some("message_stop") => {
                out.push(StreamEvent::Done {
                    stop_reason: self.stop_reason.clone(),
                });
            }
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ProviderConfig {
        ProviderConfig {
            kind: super::super::ProviderKind::Anthropic,
            model: "claude-opus-5".into(),
            api_key: "AKEY".into(),
            base_url: None,
            enable_web_search: true,
            max_output_tokens: 512,
            effort: Some("low".into()),
            thinking_budget: None,
            thinking_level: None,
        }
    }

    fn req() -> ChatRequest {
        ChatRequest {
            system: "SYS".into(),
            context_block: "CTX".into(),
            turns: vec![
                Turn::User { text: "q1".into(), images: vec![] },
                Turn::Assistant { text: "a1".into() },
                Turn::SystemNote { text: "memories: dex build".into() },
                Turn::User {
                    text: "what now?".into(),
                    images: vec![super::super::ImageAttachment {
                        media_type: "image/jpeg".into(),
                        base64_data: "QUJD".into(),
                    }],
                },
            ],
        }
    }

    #[test]
    fn builds_messages_request_with_caching_effort_and_search() {
        let p = Anthropic::default();
        let spec = p.build_request(&cfg(), &req());
        assert_eq!(spec.url, "https://api.anthropic.com/v1/messages");
        assert!(spec.headers.iter().any(|(k, v)| k == "x-api-key" && v == "AKEY"));
        assert!(spec.headers.iter().any(|(k, v)| k == "anthropic-version" && v == API_VERSION));
        // Two cached system blocks with 1h TTL.
        assert_eq!(spec.body["system"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(spec.body["system"][1]["text"], "CTX");
        // Mid-conversation system note kept as its own role:"system" message.
        let msgs = spec.body["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["role"], "system");
        // Image block shape + moving breakpoint on the last block of last turn.
        let last_content = msgs[3]["content"].as_array().unwrap();
        assert_eq!(last_content[0]["source"]["media_type"], "image/jpeg");
        assert_eq!(last_content[1]["cache_control"]["type"], "ephemeral");
        assert_eq!(spec.body["output_config"]["effort"], "low");
        assert_eq!(spec.body["tools"][0]["type"], "web_search_20260209");
    }

    #[test]
    fn parses_anthropic_stream() {
        let mut p = Anthropic::default();
        let mk = |d: &str| SseMessage { event: None, data: d.to_string() };
        assert!(p
            .parse_sse(&mk(
                r#"{"type":"message_start","message":{"usage":{"input_tokens":900,"cache_read_input_tokens":800}}}"#
            ))
            .is_empty());
        let e = p.parse_sse(&mk(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Yo"}}"#,
        ));
        assert_eq!(e, vec![StreamEvent::TextDelta("Yo".into())]);
        let e = p.parse_sse(&mk(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#,
        ));
        assert_eq!(
            e,
            vec![StreamEvent::Usage { input_tokens: 900, output_tokens: 42, cache_read_tokens: 800 }]
        );
        let e = p.parse_sse(&mk(r#"{"type":"message_stop"}"#));
        assert_eq!(e, vec![StreamEvent::Done { stop_reason: Some("end_turn".into()) }]);
    }

    #[test]
    fn tool_use_start_surfaces_as_activity() {
        let mut p = Anthropic::default();
        let e = p.parse_sse(&SseMessage {
            event: Some("content_block_start".into()),
            data: r#"{"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"tu_1","name":"web_search"}}"#.into(),
        });
        assert_eq!(e, vec![StreamEvent::ToolActivity("web_search".into())]);
    }
}
