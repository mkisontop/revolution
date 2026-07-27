//! Channel message types that connect the OS glue threads to the
//! orchestrator run-loop. Everything funnels into `LoopMsg`; the run-loop is
//! the only place `revolution_core::orchestrator::Orchestrator` is touched.

use revolution_core::orchestrator::InputEvent;

/// Everything the run-loop can receive.
#[derive(Debug)]
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
