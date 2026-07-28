//! BYO-model provider layer (DESIGN.md §6.0).
//!
//! Each adapter is **sans-IO**: it builds the exact HTTP request for its wire
//! protocol and parses that protocol's SSE stream into normalized
//! [`StreamEvent`]s. The desktop shell owns the actual transport. This is
//! what makes "plug in your own endpoint + key + model" testable without
//! network access.

pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod realtime;
pub mod sse;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI-compatible chat-completions endpoints: OpenAI itself, xAI Grok
    /// (`https://api.x.ai/v1`), OpenRouter, Groq, local servers, …
    OpenaiCompat,
    /// Google Gemini (`generativelanguage.googleapis.com`).
    Gemini,
    /// Anthropic Claude Messages API.
    Anthropic,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    /// Model ID exactly as the endpoint expects it (user-chosen, never hardcoded).
    pub model: String,
    pub api_key: String,
    /// Override for OpenAI-compatible/self-hosted endpoints; providers have
    /// sensible defaults when omitted.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Enable the provider's native web-search tool (DESIGN.md §6.1b).
    #[serde(default)]
    pub enable_web_search: bool,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    /// Reasoning effort: "minimal"/"low"/"medium"/"high". Sent as
    /// `output_config.effort` (Anthropic) or `reasoning_effort`
    /// (OpenAI-compatible, incl. GPT-5.x via OpenRouter or a local router).
    /// Unset means the model's default, which for reasoning models is slow.
    #[serde(default)]
    pub effort: Option<String>,
    /// Gemini 2.5-era knob (`thinkingConfig.thinkingBudget`): 0 disables
    /// thinking, -1 = dynamic, N = token budget. Ignored elsewhere.
    #[serde(default)]
    pub thinking_budget: Option<i32>,
    /// Gemini 3.x knob (`thinkingConfig.thinkingLevel`): "minimal" keeps the
    /// hot voice lane inside the 2s budget. Ignored elsewhere.
    #[serde(default)]
    pub thinking_level: Option<String>,
}

fn default_max_output_tokens() -> u32 {
    1024
}

/// Debug must never leak the API key into logs.
impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("kind", &self.kind)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("enable_web_search", &self.enable_web_search)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("effort", &self.effort)
            .field("thinking_budget", &self.thinking_budget)
            .field("thinking_level", &self.thinking_level)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageAttachment {
    /// e.g. "image/jpeg"
    pub media_type: String,
    pub base64_data: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Turn {
    User {
        text: String,
        images: Vec<ImageAttachment>,
    },
    Assistant {
        text: String,
    },
    /// Late-injected operator/state note (game switched, memory retrievals).
    /// Mapped to the closest concept each provider has (DESIGN.md §6.2).
    SystemNote {
        text: String,
    },
}

/// Provider-neutral request produced by the prompt builder.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatRequest {
    /// Frozen core system prompt (byte-identical across a session — cache anchor).
    pub system: String,
    /// Per-game profile + session summary block (changes only at boundaries).
    pub context_block: String,
    pub turns: Vec<Turn>,
}

/// A fully-specified HTTP request the shell can execute verbatim.
#[derive(Clone, Debug, PartialEq)]
pub struct HttpRequestSpec {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
}

/// Normalized stream events every adapter emits.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    /// Provider is running a server-side tool (web search etc.) — drives the
    /// pet's "looking it up 🔍" state.
    ToolActivity(String),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
    },
    Done {
        stop_reason: Option<String>,
    },
}

/// Streaming adapters keep a little per-response state (finish reasons,
/// usage accumulation), so parsing is `&mut self`. Create one per request.
/// `Send` so the shell can drive the stream from an async task.
pub trait ChatProvider: Send {
    fn build_request(&self, cfg: &ProviderConfig, req: &ChatRequest) -> HttpRequestSpec;
    fn parse_sse(&mut self, msg: &sse::SseMessage) -> Vec<StreamEvent>;
}

pub fn provider_for(kind: ProviderKind) -> Box<dyn ChatProvider> {
    match kind {
        ProviderKind::OpenaiCompat => Box::new(openai::OpenAiCompat::default()),
        ProviderKind::Gemini => Box::new(gemini::Gemini::default()),
        ProviderKind::Anthropic => Box::new(anthropic::Anthropic::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_leaks_api_key() {
        let cfg = ProviderConfig {
            kind: ProviderKind::OpenaiCompat,
            model: "some-model".into(),
            api_key: "sk-SUPER-SECRET".into(),
            base_url: None,
            enable_web_search: false,
            max_output_tokens: 512,
            effort: None,
            thinking_budget: None,
            thinking_level: None,
        };
        let s = format!("{cfg:?}");
        assert!(!s.contains("SUPER-SECRET"));
        assert!(s.contains("<redacted>"));
    }
}
