//! Cache-friendly prompt assembly + transcript compaction (DESIGN.md §6.2–6.3).
//!
//! Invariants enforced here (and by tests):
//! - the frozen system core is byte-identical across a session;
//! - the context block changes only at explicit boundaries;
//! - images appear only in the newest turns and are capped per question;
//! - volatile state (memories, game switches) is injected late as
//!   `SystemNote` turns, never into the frozen prefix;
//! - compaction converts old turns (and their images) into one summary note.

use crate::providers::{ChatRequest, ImageAttachment, Turn};

/// Rough image token estimate ((w×h)/750 at ~1280×720 ≈ 1.2–1.6K).
pub const IMAGE_TOKEN_ESTIMATE: usize = 1_300;

#[derive(Clone, Debug)]
pub struct PromptConfig {
    pub max_images_per_question: usize,
    /// Turns kept verbatim through a compaction.
    pub keep_recent_turns: usize,
    pub compact_after_est_tokens: usize,
    pub compact_after_images: usize,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            max_images_per_question: 3,
            keep_recent_turns: 4,
            compact_after_est_tokens: 40_000,
            compact_after_images: 10,
        }
    }
}

fn turn_est_tokens(t: &Turn) -> usize {
    match t {
        Turn::User { text, images } => text.len() / 4 + images.len() * IMAGE_TOKEN_ESTIMATE,
        Turn::Assistant { text } | Turn::SystemNote { text } => text.len() / 4,
    }
}

#[derive(Default, Clone)]
pub struct Transcript {
    turns: Vec<Turn>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_user(&mut self, text: impl Into<String>, images: Vec<ImageAttachment>) {
        self.turns.push(Turn::User { text: text.into(), images });
    }

    pub fn push_assistant(&mut self, text: impl Into<String>) {
        self.turns.push(Turn::Assistant { text: text.into() });
    }

    pub fn push_note(&mut self, text: impl Into<String>) {
        self.turns.push(Turn::SystemNote { text: text.into() });
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn est_tokens(&self) -> usize {
        self.turns.iter().map(turn_est_tokens).sum()
    }

    pub fn image_count(&self) -> usize {
        self.turns
            .iter()
            .map(|t| match t {
                Turn::User { images, .. } => images.len(),
                _ => 0,
            })
            .sum()
    }

    pub fn needs_compaction(&self, cfg: &PromptConfig) -> bool {
        self.est_tokens() > cfg.compact_after_est_tokens
            || self.image_count() > cfg.compact_after_images
    }

    /// Replace everything except the last `keep_recent_turns` turns with a
    /// single summary note; old images inside the kept window older than the
    /// newest user turn are downgraded to text placeholders. Returns the
    /// number of turns that were folded into the summary.
    ///
    /// The `summary` is produced by the model (and is also written to the
    /// memory store by the caller) — this method only performs the splice.
    pub fn compact(&mut self, cfg: &PromptConfig, summary: &str) -> usize {
        let keep_from = self.turns.len().saturating_sub(cfg.keep_recent_turns);
        let folded = keep_from;
        let mut kept = self.turns.split_off(keep_from);

        // Strip images from all kept user turns except the newest one.
        if let Some(newest_user_idx) = kept.iter().rposition(|t| matches!(t, Turn::User { .. })) {
            for (i, t) in kept.iter_mut().enumerate() {
                if i == newest_user_idx {
                    continue;
                }
                if let Turn::User { text, images } = t {
                    if !images.is_empty() {
                        *text = format!("{text}\n[{} earlier screenshot(s) removed]", images.len());
                        images.clear();
                    }
                }
            }
        }

        let mut new_turns =
            vec![Turn::SystemNote { text: format!("Session so far (compacted): {summary}") }];
        new_turns.append(&mut kept);
        self.turns = new_turns;
        folded
    }
}

pub struct PromptBuilder {
    /// Frozen persona/system core — never varies within a session.
    pub system_core: String,
}

impl PromptBuilder {
    pub fn new(system_core: impl Into<String>) -> Self {
        Self { system_core: system_core.into() }
    }

    /// `today` is the real local date, e.g. "Monday, July 28 2026" — models
    /// otherwise guess dates from their training cutoff. It lives in the
    /// context block (stable within a session; rolls at midnight), never in
    /// the frozen system core.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        &self,
        cfg: &PromptConfig,
        today: Option<&str>,
        profile_md: &str,
        last_episode: Option<&str>,
        retrieved_facts: &[String],
        transcript: &Transcript,
        question: &str,
        frames: Vec<ImageAttachment>,
    ) -> ChatRequest {
        let mut turns = transcript.turns().to_vec();
        if !retrieved_facts.is_empty() {
            turns.push(Turn::SystemNote {
                text: format!("Relevant memories:\n- {}", retrieved_facts.join("\n- ")),
            });
        }
        let frames: Vec<ImageAttachment> = frames
            .into_iter()
            .take(cfg.max_images_per_question)
            .collect();
        turns.push(Turn::User { text: question.to_string(), images: frames });

        let mut context_block = String::new();
        if let Some(t) = today {
            context_block.push_str(&format!("## Today\n{t}\n\n"));
        }
        context_block.push_str(&format!("## Player profile\n{profile_md}\n"));
        if let Some(ep) = last_episode {
            context_block.push_str(&format!("\n## Last session\n{ep}\n"));
        }

        ChatRequest {
            system: self.system_core.clone(),
            context_block,
            turns,
        }
    }
}

/// The default frozen persona core (v0). Kept here so every surface (hot
/// lane, realtime session instructions, deep lane variants) derives from one
/// source of truth.
pub fn default_system_core() -> String {
    "\
You are Rev, an expert gaming companion — a sharp, warm friend sitting next to the player, \
watching their screen. You can see recent screenshots of their game, you remember them \
(profile and memories are provided), and you can search the web when current game knowledge \
is needed.

Voice shape (these are spoken out loud):
- React first, advise second — a friend goes \"oh that was rough\" before the tip.
- Answer first, details after. 2–4 sentences max for answers; banter is 1–2 sentences.
- At most one question per reply. No bullet lists, no markdown or asterisks \
(every word is spoken aloud), no disclaimers, no \"as an AI\" — ever.
- Sound like a friend on the couch, not a manual. Roasts welcome when earned; never condescending.

Rules:
- Ground answers in what is actually visible in the screenshots; say so when you are guessing.
- Getting game mechanics wrong is worse than pausing: if patch-specific or numeric facts \
matter and you are not certain, say you're double-checking and use web search — never bluff.
- Spoiler guard: never reveal plot or future content beyond where the player is unless they \
explicitly ask.
- Use the remember/update_profile tools for durable facts about the player (build, goals, \
preferences, running jokes) — not for trivia.
- Never claim you saw something you did not. Honesty over vibes.\
"
    .to_string()
}

/// Instruction for the background summarizer (session episodes + transcript
/// compaction). Third-person past tense so the summary reads correctly when
/// injected into a later session's context block.
pub fn summarizer_instruction() -> String {
    "\
Summarize this gaming session transcript in 3-5 plain sentences. Capture what \
actually matters for picking the session back up later: the player's build or \
loadout, stated goals, key events (bosses, deaths, catches, unlocks), decisions \
made, and any running jokes. Past tense, third person ('the player'). No \
markdown, no preamble — output only the summary sentences.\
"
    .to_string()
}

/// System core for the opt-in ambient (hype-mode) lane. The cadence contract
/// is the load-bearing part: unprompted remarks land only in natural pauses.
pub fn ambient_system_core() -> String {
    "\
You are Rev in ambient mode: an occasional one-liner from the friend on the couch, based on \
the latest keyframe of the player's game.

The cadence contract (non-negotiable):
- Speak only in natural pauses: between rounds, after a death or respawn, during travel, \
menus, loading screens, or quiet farming. NEVER over an active fight or clutch moment.
- If the frame looks like active combat or high tension, output nothing at all.
- One sentence, two at most. React like a person ('clean fight', 'you're rich now — maybe \
upgrade that armor?'), don't coach unless something is obviously being wasted.
- Moments worth a remark: a win/clutch just resolved, a death worth a gentle roast, loot or \
level milestones, long silence during farming, a return to base after an expedition.
- Never repeat a remark style twice in a row; silence is always acceptable output.\
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img() -> ImageAttachment {
        ImageAttachment { media_type: "image/jpeg".into(), base64_data: "QUJD".into() }
    }

    fn builder() -> PromptBuilder {
        PromptBuilder::new(default_system_core())
    }

    #[test]
    fn frozen_prefix_is_byte_stable_across_turns() {
        let b = builder();
        let cfg = PromptConfig::default();
        let today = Some("Monday, July 28 2026");
        let mut tr = Transcript::new();
        let r1 = b.build(&cfg, today, "profile v1", Some("ep1"), &[], &tr, "q1", vec![img()]);
        tr.push_user("q1", vec![]);
        tr.push_assistant("a1");
        let r2 = b.build(&cfg, today, "profile v1", Some("ep1"), &[], &tr, "q2", vec![img()]);
        // Cache anchors must be byte-identical while the session context is stable.
        assert_eq!(r1.system, r2.system);
        assert_eq!(r1.context_block, r2.context_block);
    }

    #[test]
    fn real_date_lands_in_context_block_not_system() {
        let b = builder();
        let cfg = PromptConfig::default();
        let tr = Transcript::new();
        let r = b.build(&cfg, Some("Tuesday, July 28 2026"), "p", None, &[], &tr, "what day is it?", vec![]);
        assert!(r.context_block.contains("## Today\nTuesday, July 28 2026"));
        assert!(!r.system.contains("July 28"), "date must not poison the frozen core");
        // Omitted → no Today section at all.
        let r = b.build(&cfg, None, "p", None, &[], &tr, "q", vec![]);
        assert!(!r.context_block.contains("## Today"));
    }

    #[test]
    fn images_are_capped_and_only_in_newest_turn() {
        let b = builder();
        let cfg = PromptConfig::default();
        let tr = Transcript::new();
        let r = b.build(&cfg, None, "p", None, &[], &tr, "q", vec![img(), img(), img(), img(), img()]);
        let Turn::User { images, .. } = r.turns.last().unwrap() else {
            panic!("last turn must be the user question");
        };
        assert_eq!(images.len(), cfg.max_images_per_question);
    }

    #[test]
    fn retrieved_memories_are_injected_late_not_in_prefix() {
        let b = builder();
        let cfg = PromptConfig::default();
        let tr = Transcript::new();
        let facts = vec!["dex build, level 34".to_string()];
        let r = b.build(&cfg, None, "p", None, &facts, &tr, "q", vec![]);
        assert!(!r.system.contains("dex build"));
        assert!(!r.context_block.contains("dex build"));
        let note_idx = r.turns.iter().position(|t| matches!(t, Turn::SystemNote { text } if text.contains("dex build")));
        assert_eq!(note_idx, Some(r.turns.len() - 2), "memories go right before the question");
    }

    #[test]
    fn compaction_triggers_and_shrinks() {
        let cfg = PromptConfig { compact_after_images: 3, ..Default::default() };
        let mut tr = Transcript::new();
        for i in 0..6 {
            tr.push_user(format!("question {i}"), vec![img()]);
            tr.push_assistant(format!("answer {i}"));
        }
        assert!(tr.needs_compaction(&cfg));
        let before_tokens = tr.est_tokens();
        let folded = tr.compact(&cfg, "player asked about pals and upgraded base");
        assert_eq!(folded, 12 - cfg.keep_recent_turns);
        assert!(tr.est_tokens() < before_tokens);
        // First turn is now the summary note.
        assert!(matches!(&tr.turns()[0], Turn::SystemNote { text } if text.contains("compacted")));
        // Only the newest kept user turn may still hold images.
        let users_with_images = tr
            .turns()
            .iter()
            .filter(|t| matches!(t, Turn::User { images, .. } if !images.is_empty()))
            .count();
        assert!(users_with_images <= 1);
    }

    #[test]
    fn token_estimate_counts_images() {
        let mut tr = Transcript::new();
        tr.push_user("hello", vec![img(), img()]);
        assert!(tr.est_tokens() >= 2 * IMAGE_TOKEN_ESTIMATE);
        assert_eq!(tr.image_count(), 2);
    }
}
