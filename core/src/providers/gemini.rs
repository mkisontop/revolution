//! Google Gemini adapter (`streamGenerateContent?alt=sse`).
//!
//! Native web search = the `google_search` grounding tool. Gemini has no
//! mid-conversation system role, so `SystemNote` turns are wrapped as
//! `<system_note>` user parts (DESIGN.md §6.2).

use serde_json::{json, Value};

use super::sse::SseMessage;
use super::{ChatProvider, ChatRequest, HttpRequestSpec, ProviderConfig, StreamEvent, Turn};

pub const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com";

#[derive(Default)]
pub struct Gemini {
    finished: bool,
}

impl ChatProvider for Gemini {
    fn build_request(&self, cfg: &ProviderConfig, req: &ChatRequest) -> HttpRequestSpec {
        let base = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            base.trim_end_matches('/'),
            cfg.model
        );

        let mut contents: Vec<Value> = Vec::new();
        for t in &req.turns {
            match t {
                Turn::User { text, images } => {
                    let mut parts = vec![json!({"text": text})];
                    for im in images {
                        parts.push(json!({
                            "inline_data": {"mime_type": im.media_type, "data": im.base64_data}
                        }));
                    }
                    contents.push(json!({"role": "user", "parts": parts}));
                }
                Turn::Assistant { text } => {
                    contents.push(json!({"role": "model", "parts": [{"text": text}]}));
                }
                Turn::SystemNote { text } => {
                    contents.push(json!({
                        "role": "user",
                        "parts": [{"text": format!("<system_note>{}</system_note>", text)}]
                    }));
                }
            }
        }

        let mut body = json!({
            "system_instruction": {"parts": [{"text": format!("{}\n\n{}", req.system, req.context_block)}]},
            "contents": contents,
            "generationConfig": {"maxOutputTokens": cfg.max_output_tokens},
        });
        if cfg.enable_web_search {
            body["tools"] = json!([{"google_search": {}}]);
        }

        HttpRequestSpec {
            method: "POST",
            url,
            headers: vec![
                ("x-goog-api-key".into(), cfg.api_key.clone()),
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
        if let Some(parts) = v.pointer("/candidates/0/content/parts").and_then(Value::as_array) {
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        out.push(StreamEvent::TextDelta(t.to_string()));
                    }
                }
            }
        }
        // Search grounding shows up as groundingMetadata on candidates.
        if v.pointer("/candidates/0/groundingMetadata").is_some() {
            out.push(StreamEvent::ToolActivity("google_search".into()));
        }
        if let Some(um) = v.get("usageMetadata") {
            out.push(StreamEvent::Usage {
                input_tokens: um.get("promptTokenCount").and_then(Value::as_u64).unwrap_or(0),
                output_tokens: um
                    .get("candidatesTokenCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_read_tokens: um
                    .get("cachedContentTokenCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            });
        }
        if let Some(fr) = v.pointer("/candidates/0/finishReason").and_then(Value::as_str) {
            if !self.finished {
                out.push(StreamEvent::Done { stop_reason: Some(fr.to_string()) });
                self.finished = true;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(search: bool) -> ProviderConfig {
        ProviderConfig {
            kind: super::super::ProviderKind::Gemini,
            model: "gemini-test-flash".into(),
            api_key: "GKEY".into(),
            base_url: None,
            enable_web_search: search,
            max_output_tokens: 256,
            effort: None,
        }
    }

    fn simple_req() -> ChatRequest {
        ChatRequest {
            system: "SYS".into(),
            context_block: "CTX".into(),
            turns: vec![
                Turn::User { text: "hi".into(), images: vec![] },
                Turn::Assistant { text: "hello!".into() },
                Turn::SystemNote { text: "note".into() },
                Turn::User {
                    text: "look".into(),
                    images: vec![super::super::ImageAttachment {
                        media_type: "image/jpeg".into(),
                        base64_data: "QUJD".into(),
                    }],
                },
            ],
        }
    }

    #[test]
    fn builds_gemini_request_shape() {
        let p = Gemini::default();
        let spec = p.build_request(&cfg(true), &simple_req());
        assert_eq!(
            spec.url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-test-flash:streamGenerateContent?alt=sse"
        );
        assert!(spec.headers.iter().any(|(k, v)| k == "x-goog-api-key" && v == "GKEY"));
        assert!(spec.body["system_instruction"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("SYS"));
        let contents = spec.body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 4);
        assert_eq!(contents[1]["role"], "model");
        assert!(contents[2]["parts"][0]["text"].as_str().unwrap().contains("<system_note>"));
        assert_eq!(contents[3]["parts"][1]["inline_data"]["mime_type"], "image/jpeg");
        assert_eq!(spec.body["tools"][0]["google_search"], json!({}));
        assert_eq!(spec.body["generationConfig"]["maxOutputTokens"], 256);
    }

    #[test]
    fn parses_gemini_stream() {
        let mut p = Gemini::default();
        let mk = |d: &str| SseMessage { event: None, data: d.to_string() };
        let e1 = p.parse_sse(&mk(r#"{"candidates":[{"content":{"parts":[{"text":"Hey "}],"role":"model"}}]}"#));
        assert_eq!(e1, vec![StreamEvent::TextDelta("Hey ".into())]);
        let e2 = p.parse_sse(&mk(
            r#"{"candidates":[{"content":{"parts":[{"text":"there"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":4,"cachedContentTokenCount":6}}"#,
        ));
        assert_eq!(
            e2,
            vec![
                StreamEvent::TextDelta("there".into()),
                StreamEvent::Usage { input_tokens: 12, output_tokens: 4, cache_read_tokens: 6 },
                StreamEvent::Done { stop_reason: Some("STOP".into()) },
            ]
        );
    }
}
