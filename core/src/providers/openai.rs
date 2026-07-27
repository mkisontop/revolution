//! OpenAI-compatible chat-completions adapter.
//!
//! Covers OpenAI itself plus every endpoint speaking the same wire format:
//! xAI Grok (`https://api.x.ai/v1`), OpenRouter, Groq, local servers.
//! Web-search wiring differs by host: xAI uses `search_parameters` (Live
//! Search); on api.openai.com we set `web_search_options` (search-preview
//! models on chat-completions; the Responses-API variant is a Phase 3 item).

use serde_json::{json, Value};

use super::sse::SseMessage;
use super::{ChatProvider, ChatRequest, HttpRequestSpec, ProviderConfig, StreamEvent, Turn};

pub const DEFAULT_BASE: &str = "https://api.openai.com/v1";

#[derive(Default)]
pub struct OpenAiCompat {
    finish_reason: Option<String>,
    done_emitted: bool,
}

impl ChatProvider for OpenAiCompat {
    fn build_request(&self, cfg: &ProviderConfig, req: &ChatRequest) -> HttpRequestSpec {
        let base = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        let url = format!("{}/chat/completions", base.trim_end_matches('/'));

        let mut messages = vec![json!({
            "role": "system",
            "content": format!("{}\n\n{}", req.system, req.context_block),
        })];
        for t in &req.turns {
            match t {
                Turn::User { text, images } if images.is_empty() => {
                    messages.push(json!({"role": "user", "content": text}));
                }
                Turn::User { text, images } => {
                    let mut parts = vec![json!({"type": "text", "text": text})];
                    for im in images {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:{};base64,{}", im.media_type, im.base64_data)},
                        }));
                    }
                    messages.push(json!({"role": "user", "content": parts}));
                }
                Turn::Assistant { text } => {
                    messages.push(json!({"role": "assistant", "content": text}));
                }
                Turn::SystemNote { text } => {
                    messages.push(json!({"role": "system", "content": text}));
                }
            }
        }

        let mut body = json!({
            "model": cfg.model,
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_tokens": cfg.max_output_tokens,
            "messages": messages,
        });
        if cfg.enable_web_search {
            if base.to_lowercase().contains("x.ai") {
                body["search_parameters"] = json!({"mode": "auto"});
            } else {
                body["web_search_options"] = json!({});
            }
        }

        HttpRequestSpec {
            method: "POST",
            url,
            headers: vec![
                ("authorization".into(), format!("Bearer {}", cfg.api_key)),
                ("content-type".into(), "application/json".into()),
            ],
            body,
        }
    }

    fn parse_sse(&mut self, msg: &SseMessage) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if msg.data.trim() == "[DONE]" {
            if !self.done_emitted {
                out.push(StreamEvent::Done {
                    stop_reason: self.finish_reason.clone(),
                });
                self.done_emitted = true;
            }
            return out;
        }
        let Ok(v) = serde_json::from_str::<Value>(&msg.data) else {
            return out;
        };
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            out.push(StreamEvent::Usage {
                input_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
                output_tokens: u
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_read_tokens: u
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            });
        }
        if let Some(choice) = v.pointer("/choices/0") {
            if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str) {
                if !text.is_empty() {
                    out.push(StreamEvent::TextDelta(text.to_string()));
                }
            }
            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(fr.to_string());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(base: Option<&str>, search: bool) -> ProviderConfig {
        ProviderConfig {
            kind: super::super::ProviderKind::OpenaiCompat,
            model: "test-model".into(),
            api_key: "KEY".into(),
            base_url: base.map(String::from),
            enable_web_search: search,
            max_output_tokens: 777,
            effort: None,
            thinking_budget: None,
            thinking_level: None,
        }
    }

    fn req_with_image() -> ChatRequest {
        ChatRequest {
            system: "SYS".into(),
            context_block: "PROFILE".into(),
            turns: vec![
                Turn::SystemNote { text: "game switched to Palworld".into() },
                Turn::User {
                    text: "best pal for this?".into(),
                    images: vec![super::super::ImageAttachment {
                        media_type: "image/jpeg".into(),
                        base64_data: "QUJD".into(),
                    }],
                },
            ],
        }
    }

    #[test]
    fn builds_openai_request_shape() {
        let p = OpenAiCompat::default();
        let spec = p.build_request(&cfg(None, false), &req_with_image());
        assert_eq!(spec.url, "https://api.openai.com/v1/chat/completions");
        assert_eq!(spec.body["model"], "test-model");
        assert_eq!(spec.body["stream"], true);
        assert_eq!(spec.body["max_tokens"], 777);
        // system prompt + note + user
        let msgs = spec.body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "system");
        assert!(msgs[0]["content"].as_str().unwrap().starts_with("SYS"));
        assert_eq!(msgs[1]["role"], "system");
        let user_parts = msgs[2]["content"].as_array().unwrap();
        assert_eq!(user_parts[1]["image_url"]["url"], "data:image/jpeg;base64,QUJD");
        assert!(spec.headers.iter().any(|(k, v)| k == "authorization" && v == "Bearer KEY"));
        assert!(spec.body.get("web_search_options").is_none());
    }

    #[test]
    fn grok_base_url_gets_live_search_params() {
        let p = OpenAiCompat::default();
        let spec = p.build_request(&cfg(Some("https://api.x.ai/v1"), true), &req_with_image());
        assert_eq!(spec.url, "https://api.x.ai/v1/chat/completions");
        assert_eq!(spec.body["search_parameters"]["mode"], "auto");
        assert!(spec.body.get("web_search_options").is_none());
    }

    #[test]
    fn openai_base_gets_web_search_options() {
        let p = OpenAiCompat::default();
        let spec = p.build_request(&cfg(None, true), &req_with_image());
        assert!(spec.body.get("web_search_options").is_some());
        assert!(spec.body.get("search_parameters").is_none());
    }

    #[test]
    fn parses_stream_deltas_usage_and_done() {
        let mut p = OpenAiCompat::default();
        let mk = |d: &str| SseMessage { event: None, data: d.to_string() };
        let e1 = p.parse_sse(&mk(r#"{"choices":[{"delta":{"content":"Hel"},"finish_reason":null}]}"#));
        assert_eq!(e1, vec![StreamEvent::TextDelta("Hel".into())]);
        let e2 = p.parse_sse(&mk(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#));
        assert!(e2.is_empty());
        let e3 = p.parse_sse(&mk(
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":80}}}"#,
        ));
        assert_eq!(
            e3,
            vec![StreamEvent::Usage { input_tokens: 100, output_tokens: 9, cache_read_tokens: 80 }]
        );
        let e4 = p.parse_sse(&mk("[DONE]"));
        assert_eq!(e4, vec![StreamEvent::Done { stop_reason: Some("stop".into()) }]);
        assert!(p.parse_sse(&mk("[DONE]")).is_empty(), "Done only once");
    }
}
