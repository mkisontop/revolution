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
    pub privacy: Privacy,
    #[serde(default)]
    pub budget: Budget,
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
    /// Pipeline B: speech-to-speech (DESIGN.md §7.4).
    #[serde(default)]
    pub realtime: Option<RealtimeRole>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealtimeRole {
    pub model: String,
    #[serde(default = "default_voice")]
    pub voice: String,
    pub api_key: String,
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
}

impl Default for Budget {
    fn default() -> Self {
        Self { monthly_usd_cap: default_cap() }
    }
}

fn default_cap() -> f64 {
    25.0
}

fn default_true() -> bool {
    true
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
# effort = "low"                  # snappy voice turns
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

[privacy]
upload_on_ask_only = true
hype_mode = false
capture_indicator = true
game_foreground_only = true

[budget]
monthly_usd_cap = 25.0
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
    }
}
