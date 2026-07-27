//! Pipeline B (DESIGN.md §7.4): OpenAI Realtime speech-to-speech session
//! configuration. The shell owns the WebRTC/WebSocket transport and audio;
//! this module only builds the session payload so its shape is testable.
//!
//! NOTE: verify field names against the current Realtime API reference at
//! integration time (Phase 3) — the surface is newer than the rest of this
//! crate and evolves faster.

use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct RealtimeSessionConfig {
    /// e.g. "gpt-realtime-2.1"
    pub model: String,
    pub voice: String,
    /// Same frozen persona core + per-game context as Pipeline A.
    pub instructions: String,
    pub enable_web_search: bool,
}

/// Payload for a `session.update` event after connecting.
pub fn session_update_payload(cfg: &RealtimeSessionConfig) -> Value {
    let mut session = json!({
        "model": cfg.model,
        "instructions": cfg.instructions,
        "audio": {"output": {"voice": cfg.voice}},
    });
    if cfg.enable_web_search {
        session["tools"] = json!([{"type": "web_search"}]);
    }
    json!({"type": "session.update", "session": session})
}

/// Attach a keyframe into the live conversation (image input to the session).
pub fn image_item_payload(media_type: &str, base64_data: &str) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_image", "image_url": format!("data:{};base64,{}", media_type, base64_data)}
            ]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_payload_shape() {
        let p = session_update_payload(&RealtimeSessionConfig {
            model: "gpt-realtime-2.1".into(),
            voice: "verse".into(),
            instructions: "be a friendly gaming buddy".into(),
            enable_web_search: true,
        });
        assert_eq!(p["type"], "session.update");
        assert_eq!(p["session"]["model"], "gpt-realtime-2.1");
        assert_eq!(p["session"]["audio"]["output"]["voice"], "verse");
        assert_eq!(p["session"]["tools"][0]["type"], "web_search");
    }

    #[test]
    fn image_item_shape() {
        let p = image_item_payload("image/jpeg", "QUJD");
        assert_eq!(p["item"]["content"][0]["type"], "input_image");
        assert!(p["item"]["content"][0]["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/jpeg;base64,"));
    }
}
