//! Config file lifecycle: locate/create `%APPDATA%\revolution\config.toml`,
//! parse it with `revolution_core::config`, and resolve `keyring:` secret
//! references through Windows Credential Manager.

use std::path::PathBuf;

use revolution_core::config::AppConfig;

/// Service name under which secrets live in Windows Credential Manager.
pub const KEYRING_SERVICE: &str = "revolution";

pub struct LoadedConfig {
    pub cfg: AppConfig,
    pub path: PathBuf,
    /// Human-readable problems (unresolved keys, parse fallbacks) for the panel.
    pub warnings: Vec<String>,
    pub first_run: bool,
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("revolution")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn memory_db_path() -> PathBuf {
    config_dir().join("memory.sqlite")
}

/// Strip whitespace and invisible junk (UTF-8/UTF-16 BOMs, zero-width
/// spaces) that rides along with pasted or piped keys. A BOM in an API key
/// header cost an evening once — never again.
fn clean_secret(s: &str) -> String {
    s.trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '\u{feff}' | '\u{200b}' | '\u{200e}' | '\u{200f}')
    })
    .to_string()
}

/// Store a secret in Credential Manager under the `keyring:` namespace used
/// by config.toml (e.g. name `revolution/qa`).
pub fn save_secret(name: &str, value: &str) -> anyhow::Result<()> {
    keyring::Entry::new(KEYRING_SERVICE, name)?.set_password(&clean_secret(value))?;
    Ok(())
}

fn resolve_key(slot: &str, key: &mut String, warnings: &mut Vec<String>) {
    let Some(entry_name) = key.strip_prefix("keyring:") else {
        return; // literal key (or empty) — leave as-is
    };
    let entry_name = entry_name.trim().to_string();
    match keyring::Entry::new(KEYRING_SERVICE, &entry_name).and_then(|e| e.get_password()) {
        Ok(secret) => *key = clean_secret(&secret),
        Err(e) => warnings.push(format!(
            "{slot}: no secret stored for keyring:{entry_name} ({e}) — save it from the panel"
        )),
    }
}

/// Load config.toml, writing the annotated example on first run. Never
/// fails hard: on parse errors the example defaults are used and the error
/// is surfaced as a warning so the app still starts and the panel can help.
pub fn load_or_init() -> LoadedConfig {
    let dir = config_dir();
    let path = config_path();
    let mut warnings = Vec::new();
    let mut first_run = false;

    if let Err(e) = std::fs::create_dir_all(&dir) {
        warnings.push(format!("could not create {}: {e}", dir.display()));
    }
    if !path.exists() {
        first_run = true;
        if let Err(e) = std::fs::write(&path, AppConfig::example_toml()) {
            warnings.push(format!("could not write example config: {e}"));
        }
    }

    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut cfg = match AppConfig::from_toml(&text) {
        Ok(c) => c,
        Err(e) => {
            warnings.push(format!("config.toml failed to parse ({e}) — using defaults"));
            AppConfig::from_toml(AppConfig::example_toml()).expect("example config parses")
        }
    };

    resolve_key("roles.qa", &mut cfg.roles.qa.api_key, &mut warnings);
    if let Some(r) = cfg.roles.ambient.as_mut() {
        resolve_key("roles.ambient", &mut r.api_key, &mut warnings);
    }
    if let Some(r) = cfg.roles.deep.as_mut() {
        resolve_key("roles.deep", &mut r.api_key, &mut warnings);
    }
    if let Some(r) = cfg.roles.search.as_mut() {
        resolve_key("roles.search", &mut r.api_key, &mut warnings);
    }
    if let Some(r) = cfg.roles.realtime.as_mut() {
        resolve_key("roles.realtime", &mut r.api_key, &mut warnings);
    }
    if let Some(s) = cfg.voice.stt.as_mut() {
        resolve_key("voice.stt", &mut s.api_key, &mut warnings);
    }
    if let Some(k) = cfg.voice.tts.api_key.as_mut() {
        resolve_key("voice.tts", k, &mut warnings);
    }

    LoadedConfig { cfg, path, warnings, first_run }
}

/// True when the hot-lane key is usable (resolved to a literal secret).
pub fn qa_ready(cfg: &AppConfig) -> bool {
    let k = cfg.roles.qa.api_key.trim();
    !k.is_empty() && !k.starts_with("keyring:")
}

/// True when STT is configured with a usable key.
pub fn stt_ready(cfg: &AppConfig) -> bool {
    cfg.voice
        .stt
        .as_ref()
        .map(|s| !s.api_key.trim().is_empty() && !s.api_key.starts_with("keyring:"))
        .unwrap_or(false)
}
