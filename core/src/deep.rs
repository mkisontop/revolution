//! Deep-lane research jobs (DESIGN.md §9), Phase 3a shape: one background
//! request to the user's configured `deep` model with its native web-search
//! tool. The job queue and result cards live in the shell; this module owns
//! building a correct, personalized research request for any provider.

use crate::providers::{provider_for, ChatRequest, HttpRequestSpec, ProviderConfig, Turn};

#[derive(Clone, Debug)]
pub struct ResearchJob {
    pub game_name: String,
    pub question: String,
    /// Per-game profile card so research is personalized
    /// ("dex build — don't recommend strength weapons").
    pub profile_md: String,
    /// Optional wiki base URL hint from the games table.
    pub wiki_base_url: Option<String>,
}

pub const DEEP_SYSTEM: &str = "\
You are Rev's research mode: a meticulous game-guide researcher. Use web search to verify \
current, patch-accurate information before answering; prefer the game's official wiki and \
reputable community sources; note when sources conflict. Respect the player's profile \
(build, progression, spoiler preference). Output a compact tip card in Markdown: a one-line \
verdict first, then 3-6 actionable bullets, then source links. No filler.";

/// Build the provider-specific HTTP request for a research job.
/// Web search is force-enabled regardless of the role's default.
pub fn build_research_request(cfg: &ProviderConfig, job: &ResearchJob) -> HttpRequestSpec {
    let mut cfg = cfg.clone();
    cfg.enable_web_search = true;

    let mut context_block = format!(
        "## Game\n{}\n\n## Player profile\n{}\n",
        job.game_name, job.profile_md
    );
    if let Some(wiki) = &job.wiki_base_url {
        context_block.push_str(&format!("\n## Preferred source\n{wiki}\n"));
    }

    let req = ChatRequest {
        system: DEEP_SYSTEM.to_string(),
        context_block,
        turns: vec![Turn::User { text: job.question.clone(), images: vec![] }],
    };
    provider_for(cfg.kind).build_request(&cfg, &req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderKind;

    fn job() -> ResearchJob {
        ResearchJob {
            game_name: "Palworld".into(),
            question: "best pal for automated mining right now?".into(),
            profile_md: "mid-game, level 29 base".into(),
            wiki_base_url: Some("https://palworld.wiki.gg".into()),
        }
    }

    fn cfg(kind: ProviderKind) -> ProviderConfig {
        ProviderConfig {
            kind,
            model: "m".into(),
            api_key: "K".into(),
            base_url: None,
            enable_web_search: false, // deep lane must force this on
            max_output_tokens: 2048,
            effort: None,
            thinking_budget: None,
            thinking_level: None,
        }
    }

    #[test]
    fn research_forces_native_search_on_every_provider() {
        let spec = build_research_request(&cfg(ProviderKind::Anthropic), &job());
        assert_eq!(spec.body["tools"][0]["type"], "web_search_20260209");

        let spec = build_research_request(&cfg(ProviderKind::Gemini), &job());
        assert!(spec.body["tools"][0].get("google_search").is_some());

        let spec = build_research_request(&cfg(ProviderKind::OpenaiCompat), &job());
        assert!(spec.body.get("web_search_options").is_some());
    }

    #[test]
    fn research_request_carries_profile_and_wiki_hint() {
        let spec = build_research_request(&cfg(ProviderKind::Gemini), &job());
        let sys = spec.body["system_instruction"]["parts"][0]["text"].as_str().unwrap();
        assert!(sys.contains("research mode"));
        assert!(sys.contains("level 29 base"));
        assert!(sys.contains("palworld.wiki.gg"));
    }
}
