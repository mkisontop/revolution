//! Channel message types that connect the OS glue threads to the
//! orchestrator run-loop. Everything funnels into `LoopMsg`; the run-loop is
//! the only place `revolution_core::orchestrator::Orchestrator` is touched.

use std::sync::mpsc::Sender;

use revolution_core::memory::store::{FactRow, GameRow, MonthUsage};
use revolution_core::orchestrator::InputEvent;

/// Everything the run-loop can receive.
pub enum LoopMsg {
    /// A raw orchestrator input (hotkey, STT result, stream/audio timing).
    Input(InputEvent),
    /// Typed question from the panel — runs the same PTT path minus the mic.
    Typed(String),
    /// Streamed LLM text delta (already forwarded to TTS by the stream task);
    /// the run-loop mirrors it to the speech bubble.
    Delta(String),
    /// Provider is running a server-side tool (web search).
    ToolActivity(String),
    /// The LLM stream finished cleanly.
    LlmDone {
        full_text: String,
        usage: Option<Usage>,
        stop_reason: Option<String>,
    },
    /// Transport/provider failure — surfaces as a toast + orchestrator error.
    LlmFailed(String),
    /// STT transport failure.
    SttFailed(String),
    /// Foreground process changed (game watcher).
    GameChanged {
        exe_path: String,
        /// Some((name, exe)) when the ladder identified a game.
        game: Option<(String, String)>,
    },
    /// Panel asked to store a durable fact about the current game.
    RememberNote(String),
    /// The brain called its `remember` tool mid-answer.
    ToolRemember(String),
    /// The brain called its `update_profile` tool mid-answer.
    ToolUpdateProfile(String),
    /// Background session summarizer finished for a game episode.
    EpisodeReady {
        game_id: i64,
        started_epoch_ms: u64,
        summary: String,
    },
    /// Background compaction summarizer finished (model or heuristic).
    CompactionReady(String),
    /// Ambient hype lane produced a remark worth saying.
    AmbientRemark(String),
    /// Runtime mute toggle (tray / pet / panel).
    SetMuted(bool),
    /// Panel data query — answered via the embedded reply channel.
    Query(UiQuery),
    /// Show a toast without touching the orchestrator state.
    Toast(String),
    /// Graceful exit: flush the session episode, then terminate the app.
    Shutdown,
}

/// Panel queries that need the memory store (which lives on the run-loop
/// thread). Each carries its own reply channel; commands block briefly.
pub enum UiQuery {
    Games(Sender<Vec<GameRow>>),
    Facts {
        game_id: i64,
        reply: Sender<Vec<FactRow>>,
    },
    Forget {
        fact_id: i64,
        reply: Sender<bool>,
    },
    Profile {
        game_id: i64,
        reply: Sender<String>,
    },
    SetProfile {
        game_id: i64,
        markdown: String,
        reply: Sender<bool>,
    },
    Usage(Sender<UsageSnapshot>),
}

/// Month-to-date usage for the panel's cost meter.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageSnapshot {
    pub month: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub turns: u64,
    /// Estimated dollars, when prices are configured.
    pub est_usd: Option<f64>,
    pub cap_usd: f64,
}

impl UsageSnapshot {
    pub fn from_store(month: String, m: MonthUsage, budget: &revolution_core::config::Budget) -> Self {
        Self {
            month,
            input_tokens: m.input_tokens,
            output_tokens: m.output_tokens,
            turns: m.turns,
            est_usd: budget.estimate_usd(m.input_tokens, m.output_tokens),
            cap_usd: budget.monthly_usd_cap,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

/// Commands for the TTS synth + playback thread.
#[derive(Debug)]
pub enum TtsCmd {
    /// Synthesize and enqueue one sentence.
    Sentence(String),
    /// No more sentences for this utterance: when the sink drains, report
    /// `PlaybackFinished` to the run-loop.
    EndOfUtterance,
    /// Barge-in: stop playback now and drop anything queued.
    Stop,
}
