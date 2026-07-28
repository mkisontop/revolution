//! App configuration: BYO endpoint + key + model per role (DESIGN.md §6.1).
//!
//! Secrets: `api_key` values are redacted from Debug output (see
//! `providers::ProviderConfig`); on Windows the shell stores keys in
//! Credential Manager and injects them here at load time — the on-disk
//! `config.toml` may reference `keyring:` entries instead of literal keys.

use serde::{Deserialize, Serialize};

use crate::providers::ProviderConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub hotkey: Hotkey,
    pub roles: Roles,
    #[serde(default)]
    pub voice: Voice,
    #[serde(default)]
    pub privacy: Privacy,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub companion: Companion,
}

/// Personality knobs for the pet itself (all local behavior, no privacy
/// surface): greetings, ambient cadence, and when it dozes off.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Companion {
    /// Say a short hello when a game comes into focus.
    #[serde(default = "default_true")]
    pub greet_on_game: bool,
    /// Hype-mode cadence floor: never remark more often than this.
    #[serde(default = "default_ambient_gap")]
    pub ambient_min_gap_secs: u64,
    /// With no game and no interaction for this long, the pet falls asleep
    /// on screen (purely cosmetic). 0 disables.
    #[serde(default = "default_sleep_after")]
    pub sleep_after_secs: u64,
}

impl Default for Companion {
    fn default() -> Self {
        Self {
            greet_on_game: true,
            ambient_min_gap_secs: default_ambient_gap(),
            sleep_after_secs: default_sleep_after(),
        }
    }
}

fn default_ambient_gap() -> u64 {
    90
}

fn default_sleep_after() -> u64 {
    180
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hotkey {
    #[serde(default = "default_ptt_key")]
    pub ptt: String,
    /// false = hold-to-talk (default), true = tap to toggle.
    #[serde(default)]
    pub toggle_mode: bool,
}

impl Default for Hotkey {
    fn default() -> Self {
        Self { ptt: default_ptt_key(), toggle_mode: false }
    }
}

fn default_ptt_key() -> String {
    "PageUp".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Roles {
    /// The hot lane — required.
    pub qa: ProviderConfig,
    /// Opt-in ambient hype-mode lane.
    #[serde(default)]
    pub ambient: Option<ProviderConfig>,
    /// Async research lane.
    #[serde(default)]
    pub deep: Option<ProviderConfig>,
    /// Executes the brain's `web_search` tool calls. Defaults to the `qa`
    /// provider when it can search natively; set explicitly when the brain
    /// runs on an endpoint without web access (a local router, say).
    #[serde(default)]
    pub search: Option<ProviderConfig>,
    /// Pipeline B: speech-to-speech (DESIGN.md §7.4).
    #[serde(default)]
    pub realtime: Option<RealtimeRole>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RealtimeRole {
    pub model: String,
    #[serde(default = "default_voice")]
    pub voice: String,
    pub api_key: String,
}

/// Debug must never leak the API key into logs.
impl std::fmt::Debug for RealtimeRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealtimeRole")
            .field("model", &self.model)
            .field("voice", &self.voice)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

fn default_voice() -> String {
    "verse".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Privacy {
    /// Frames leave the machine only on explicit questions (default true).
    #[serde(default = "default_true")]
    pub upload_on_ask_only: bool,
    #[serde(default)]
    pub hype_mode: bool,
    /// The pet's capture-indicator eye (always rendered while capturing).
    #[serde(default = "default_true")]
    pub capture_indicator: bool,
    /// Stop capturing when the foreground app is not a detected game.
    #[serde(default = "default_true")]
    pub game_foreground_only: bool,
}

impl Default for Privacy {
    fn default() -> Self {
        Self {
            upload_on_ask_only: true,
            hype_mode: false,
            capture_indicator: true,
            game_foreground_only: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default = "default_cap")]
    pub monthly_usd_cap: f64,
    /// Price of the qa model per **million** input tokens, USD. Leave both
    /// prices at 0 for token-only metering with no cap enforcement (the app
    /// cannot know arbitrary models' prices).
    #[serde(default)]
    pub usd_per_1m_input: f64,
    #[serde(default)]
    pub usd_per_1m_output: f64,
}

impl Budget {
    /// Estimated cost of a month's usage; None when prices are unset.
    pub fn estimate_usd(&self, input_tokens: u64, output_tokens: u64) -> Option<f64> {
        if self.usd_per_1m_input <= 0.0 && self.usd_per_1m_output <= 0.0 {
            return None;
        }
        Some(
            input_tokens as f64 / 1e6 * self.usd_per_1m_input
                + output_tokens as f64 / 1e6 * self.usd_per_1m_output,
        )
    }

    /// True when the cap is known-exceeded (prices set and estimate ≥ cap).
    pub fn over_cap(&self, input_tokens: u64, output_tokens: u64) -> bool {
        self.monthly_usd_cap > 0.0
            && self
                .estimate_usd(input_tokens, output_tokens)
                .map(|c| c >= self.monthly_usd_cap)
                .unwrap_or(false)
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            monthly_usd_cap: default_cap(),
            usd_per_1m_input: 0.0,
            usd_per_1m_output: 0.0,
        }
    }
}

fn default_cap() -> f64 {
    25.0
}

fn default_true() -> bool {
    true
}

/// Voice I/O for the desktop shell. Everything has zero-key defaults: TTS
/// uses the built-in Windows voice, and when `stt` is absent the shell falls
/// back to typed questions in the panel.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Voice {
    #[serde(default)]
    pub stt: Option<Stt>,
    #[serde(default)]
    pub tts: Tts,
}

/// Batch speech-to-text. Two wire shapes, one config:
/// - `openai_compat_batch`: multipart `/audio/transcriptions` (Groq/OpenAI
///   Whisper).
/// - `openai_chat_audio`: the clip rides as an `input_audio` content part in
///   a chat completion (OpenRouter-style audio models — e.g.
///   `google/gemini-2.5-flash-lite`).
///
/// Streaming STT (Deepgram) layers in later.
#[derive(Clone, Serialize, Deserialize)]
pub struct Stt {
    #[serde(default)]
    pub kind: SttKind,
    #[serde(default = "default_stt_base_url")]
    pub base_url: String,
    #[serde(default = "default_stt_model")]
    pub model: String,
    pub api_key: String,
    /// ISO-639-1 hint, e.g. "en"; omit to auto-detect.
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SttKind {
    /// Multipart POST to `{base_url}/audio/transcriptions`.
    #[default]
    OpenaiCompatBatch,
    /// JSON chat completion with an `input_audio` part to
    /// `{base_url}/chat/completions`; the model replies with the transcript.
    OpenaiChatAudio,
}

/// Debug must never leak the API key into logs.
impl std::fmt::Debug for Stt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stt")
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("language", &self.language)
            .finish()
    }
}

fn default_stt_base_url() -> String {
    "https://api.groq.com/openai/v1".to_string()
}

fn default_stt_model() -> String {
    "whisper-large-v3-turbo".to_string()
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Tts {
    #[serde(default)]
    pub engine: TtsEngine,
    /// Windows: substring match against installed voice display names
    /// (e.g. "Zira"). Chat-audio: the provider voice name (e.g. "marin").
    #[serde(default)]
    pub voice: Option<String>,
    /// Windows-only speaking rate multiplier, 0.5–2.0.
    #[serde(default = "default_tts_rate")]
    pub rate: f64,
    /// `openai_chat_audio` only — OpenAI-compatible base URL.
    #[serde(default)]
    pub base_url: Option<String>,
    /// `openai_chat_audio` only — audio-output model id.
    #[serde(default)]
    pub model: Option<String>,
    /// `openai_chat_audio` only — `keyring:` refs supported.
    #[serde(default)]
    pub api_key: Option<String>,
}

impl Default for Tts {
    fn default() -> Self {
        Self {
            engine: TtsEngine::default(),
            voice: None,
            rate: default_tts_rate(),
            base_url: None,
            model: None,
            api_key: None,
        }
    }
}

/// Debug must never leak the API key into logs.
impl std::fmt::Debug for Tts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tts")
            .field("engine", &self.engine)
            .field("voice", &self.voice)
            .field("rate", &self.rate)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

fn default_tts_rate() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TtsEngine {
    /// WinRT `SpeechSynthesizer`: offline, instant, no API key.
    #[default]
    Windows,
    /// Audio-output chat model over an OpenAI-compatible endpoint
    /// (OpenRouter `openai/gpt-audio-mini` etc.): natural voices, streamed
    /// PCM16@24kHz; requires `stream: true` on OpenRouter.
    OpenaiChatAudio,
}

impl AppConfig {
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("config serializes")
    }

    /// Annotated starter config shown by the settings UI / first run.
    pub fn example_toml() -> &'static str {
        EXAMPLE_TOML
    }
}

pub const EXAMPLE_TOML: &str = r#"# Revolution config — bring your own endpoint, key, and model per role.
# Models are examples, not endorsements: set whatever your key can use.

[hotkey]
ptt = "PageUp"        # hold to talk (set toggle_mode = true to tap instead)
toggle_mode = false

# --- The live companion (required) -----------------------------------------
# Pick ONE provider block for roles.qa; delete the others.

# xAI Grok (OpenAI-compatible; needs API credits on console.x.ai):
[roles.qa]
kind = "openai_compat"
base_url = "https://api.x.ai/v1"
model = "grok-4-fast"            # any vision-capable Grok model you have
api_key = "keyring:revolution/qa" # or paste a key (kept out of logs either way)
enable_web_search = true          # Grok Live Search for fresh game knowledge
max_output_tokens = 400

# Google Gemini:
# [roles.qa]
# kind = "gemini"
# model = "gemini-flash-latest"
# api_key = "keyring:revolution/qa"
# enable_web_search = true
# thinking_level = "minimal"      # Gemini 3.x: snappy voice turns
# thinking_budget = 0             # Gemini 2.5-era equivalent

# OpenAI:
# [roles.qa]
# kind = "openai_compat"
# model = "gpt-5-mini"
# api_key = "keyring:revolution/qa"

# Anthropic Claude:
# [roles.qa]
# kind = "anthropic"
# model = "claude-opus-5"
# api_key = "keyring:revolution/qa"
# effort = "low"                  # snappy voice turns; also works on
#                                 # OpenAI-compatible reasoning models
#                                 # (sent as reasoning_effort)
# enable_web_search = true

# --- Optional lanes ---------------------------------------------------------

# [roles.ambient]                 # hype-mode one-liners (cheap model)
# kind = "gemini"
# model = "gemini-flash-lite-latest"
# api_key = "keyring:revolution/ambient"

# [roles.deep]                    # background research jobs (strong model)
# kind = "anthropic"
# model = "claude-opus-5"
# api_key = "keyring:revolution/deep"
# effort = "high"
# enable_web_search = true

# [roles.realtime]                # Pipeline B: speech-to-speech add-on
# model = "gpt-realtime-2.1"
# voice = "verse"
# api_key = "keyring:revolution/realtime"

# --- Voice I/O ---------------------------------------------------------------
# Without [voice.stt] you can still type questions in the panel window.

# Whisper batch (Groq free tier / OpenAI):
# [voice.stt]
# kind = "openai_compat_batch"
# base_url = "https://api.groq.com/openai/v1"
# model = "whisper-large-v3-turbo"
# api_key = "keyring:revolution/stt"
# language = "en"

# Audio-input chat model (OpenRouter — no Whisper endpoint there):
# [voice.stt]
# kind = "openai_chat_audio"
# base_url = "https://openrouter.ai/api/v1"
# model = "google/gemini-2.5-flash-lite"
# api_key = "keyring:revolution/stt"
# language = "en"

[voice.tts]
engine = "windows"    # built-in Windows voice: offline, instant, zero-key
# voice = "Zira"      # substring of an installed voice name; omit for default
rate = 1.0

# Natural neural voice via an audio-output chat model (falls back to the
# Windows voice automatically if the request fails):
# [voice.tts]
# engine = "openai_chat_audio"
# base_url = "https://openrouter.ai/api/v1"
# model = "openai/gpt-audio-mini"
# voice = "marin"     # or cedar, coral, sage, alloy, …
# api_key = "keyring:revolution/stt"

[privacy]
upload_on_ask_only = true
hype_mode = false         # opt-in couch commentary between fights
capture_indicator = true
game_foreground_only = true

[companion]
greet_on_game = true      # short spoken hello when your game gets focus
ambient_min_gap_secs = 90 # hype-mode never remarks more often than this
sleep_after_secs = 180    # pet dozes off with no game + no interaction

[budget]
monthly_usd_cap = 25.0
usd_per_1m_input = 0.0    # set your model's prices to enforce the cap;
usd_per_1m_output = 0.0   # 0/0 = token-only metering, no enforcement
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderKind;

    #[test]
    fn example_config_parses_with_defaults() {
        let cfg = AppConfig::from_toml(EXAMPLE_TOML).expect("example must parse");
        assert_eq!(cfg.hotkey.ptt, "PageUp");
        assert_eq!(cfg.roles.qa.kind, ProviderKind::OpenaiCompat);
        assert_eq!(cfg.roles.qa.base_url.as_deref(), Some("https://api.x.ai/v1"));
        assert!(cfg.roles.qa.enable_web_search);
        assert!(cfg.roles.ambient.is_none());
        assert!(cfg.privacy.upload_on_ask_only);
        assert_eq!(cfg.budget.monthly_usd_cap, 25.0);
        assert!(cfg.companion.greet_on_game);
        assert_eq!(cfg.companion.ambient_min_gap_secs, 90);
    }

    #[test]
    fn budget_estimates_and_cap() {
        let b = Budget { monthly_usd_cap: 5.0, usd_per_1m_input: 1.0, usd_per_1m_output: 4.0 };
        // 2M in + 0.5M out = 2 + 2 = 4 USD.
        assert_eq!(b.estimate_usd(2_000_000, 500_000), Some(4.0));
        assert!(!b.over_cap(2_000_000, 500_000));
        assert!(b.over_cap(3_000_000, 500_000));
        // Unknown prices → no estimate, never capped.
        let unknown = Budget::default();
        assert_eq!(unknown.estimate_usd(9_999_999, 9_999_999), None);
        assert!(!unknown.over_cap(u64::MAX / 2, u64::MAX / 2));
    }

    #[test]
    fn minimal_config_gets_all_defaults() {
        let cfg = AppConfig::from_toml(
            r#"
            [roles.qa]
            kind = "gemini"
            model = "gemini-flash-latest"
            api_key = "K"
            "#,
        )
        .expect("minimal parses");
        assert_eq!(cfg.hotkey.ptt, "PageUp");
        assert!(!cfg.hotkey.toggle_mode);
        assert!(cfg.privacy.capture_indicator);
        assert_eq!(cfg.roles.qa.max_output_tokens, 1024);
    }

    #[test]
    fn debug_output_redacts_keys_everywhere() {
        let cfg = AppConfig::from_toml(
            r#"
            [roles.qa]
            kind = "anthropic"
            model = "m"
            api_key = "sk-VERY-SECRET-123"
            "#,
        )
        .unwrap();
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("VERY-SECRET"));
    }

    #[test]
    fn roundtrips_through_toml() {
        let cfg = AppConfig::from_toml(EXAMPLE_TOML).unwrap();
        let re = AppConfig::from_toml(&cfg.to_toml()).expect("roundtrip");
        assert_eq!(re.roles.qa.model, cfg.roles.qa.model);
        assert_eq!(re.voice.tts.engine, cfg.voice.tts.engine);
    }

    #[test]
    fn voice_defaults_to_zero_key_windows_tts_and_no_stt() {
        let cfg = AppConfig::from_toml(
            r#"
            [roles.qa]
            kind = "gemini"
            model = "gemini-flash-latest"
            api_key = "K"
            "#,
        )
        .unwrap();
        assert!(cfg.voice.stt.is_none());
        assert_eq!(cfg.voice.tts.engine, TtsEngine::Windows);
        assert_eq!(cfg.voice.tts.rate, 1.0);
        assert!(cfg.voice.tts.voice.is_none());
    }

    #[test]
    fn chat_audio_tts_parses_and_redacts_key() {
        let cfg = AppConfig::from_toml(
            r#"
            [roles.qa]
            kind = "gemini"
            model = "m"
            api_key = "K"

            [voice.tts]
            engine = "openai_chat_audio"
            base_url = "https://openrouter.ai/api/v1"
            model = "openai/gpt-audio-mini"
            voice = "marin"
            api_key = "or-TTS-SECRET"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.voice.tts.engine, TtsEngine::OpenaiChatAudio);
        assert_eq!(cfg.voice.tts.model.as_deref(), Some("openai/gpt-audio-mini"));
        assert_eq!(cfg.voice.tts.voice.as_deref(), Some("marin"));
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("TTS-SECRET"));
    }

    #[test]
    fn stt_config_parses_with_endpoint_defaults_and_redacts_key() {
        let cfg = AppConfig::from_toml(
            r#"
            [roles.qa]
            kind = "gemini"
            model = "m"
            api_key = "K"

            [voice.stt]
            api_key = "gsk-STT-SECRET"
            "#,
        )
        .unwrap();
        let stt = cfg.voice.stt.as_ref().expect("stt present");
        assert_eq!(stt.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(stt.model, "whisper-large-v3-turbo");
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("STT-SECRET"));
    }

    #[test]
    fn realtime_role_debug_redacts_key() {
        let cfg = AppConfig::from_toml(
            r#"
            [roles.qa]
            kind = "gemini"
            model = "m"
            api_key = "K"

            [roles.realtime]
            model = "gpt-realtime-2.1"
            api_key = "rt-REALTIME-SECRET"
            "#,
        )
        .unwrap();
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("REALTIME-SECRET"));
    }
}
