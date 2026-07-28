//! The orchestrator run-loop: the single thread that owns the core state
//! machine, the transcript, and the memory store, executes the `Action`s the
//! orchestrator returns, and mirrors everything to the UI via Tauri events.
//!
//! Beyond the PTT turn cycle it also runs the companion behaviors: session
//! episode summaries, the brain's remember/update_profile tools, ambient
//! hype-mode remarks, greet-on-game, sleep/wake, the monthly budget gate,
//! and graceful shutdown.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use revolution_core::config::AppConfig;
use revolution_core::memory::embed::HashEmbedder;
use revolution_core::memory::store::{FactGateDecision, MemoryStore};
use revolution_core::orchestrator::{Action, InputEvent, Orchestrator, PetState};
use revolution_core::prompt::{
    ambient_system_core, default_system_core, PromptBuilder, PromptConfig, Transcript,
};
use revolution_core::providers::{ChatRequest, ImageAttachment, Turn};
use serde::Serialize;
use tauri::Emitter;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::capture::CaptureShared;
use crate::config_io::{self, LoadedConfig};
use crate::duck::Ducker;
use crate::llm::{self, SummarizeTarget};
use crate::mic::MicHandle;
use crate::msg::{LoopMsg, TtsCmd, UiQuery, Usage, UsageSnapshot};
use crate::stt;
use crate::util::{epoch_ms, month_string, now_ms};

/// Panel-facing status snapshot (also emitted as `rev:status`).
#[derive(Clone, Default, Serialize)]
pub struct Status {
    pub game: Option<String>,
    pub game_id: Option<i64>,
    /// Basename of the current foreground exe (helps debug game detection).
    pub foreground_exe: String,
    pub capture_enabled: bool,
    pub capture_error: Option<String>,
    pub frames_seen: u64,
    pub keyframes: u64,
    pub ring_frames: usize,
    pub ring_kb: usize,
    pub mic: String,
    pub qa_ready: bool,
    pub qa_kind: String,
    pub qa_model: String,
    pub stt_ready: bool,
    pub hotkey: String,
    pub config_path: String,
    pub warnings: Vec<String>,
    pub last_usage: Option<UsageJson>,
    pub muted: bool,
    pub hype_mode: bool,
    pub first_run: bool,
}

#[derive(Clone, Serialize)]
pub struct UsageJson {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

pub struct RunDeps {
    pub app: tauri::AppHandle,
    pub loaded: LoadedConfig,
    pub capture: Arc<CaptureShared>,
    pub mic: MicHandle,
    pub tts_tx: Sender<TtsCmd>,
    pub loop_tx: Sender<LoopMsg>,
    pub rx: Receiver<LoopMsg>,
    pub status: Arc<Mutex<Status>>,
    pub muted: Arc<AtomicBool>,
}

struct GameCtx {
    id: i64,
    name: String,
    profile: String,
    episode: Option<String>,
    /// Wall-clock session start, for the episode row.
    session_start_epoch_ms: u64,
    /// Transcript index where this game's session began.
    start_turn_idx: usize,
    /// Questions asked this session — sessions with none aren't summarized.
    asks: u32,
}

/// Rotating short hellos for greet-on-game (canned: instant + free).
const GREETS: [&str; 4] = [
    "{} — let's go!",
    "Back to {}. I'm in.",
    "Ooh, {} time. I'm watching.",
    "{}? Great choice. Let's cook.",
];

struct Loop {
    app: tauri::AppHandle,
    cfg: AppConfig,
    first_run: bool,
    capture: Arc<CaptureShared>,
    mic: MicHandle,
    tts_tx: Sender<TtsCmd>,
    loop_tx: Sender<LoopMsg>,
    status: Arc<Mutex<Status>>,
    muted: Arc<AtomicBool>,
    client: reqwest::Client,

    orch: Orchestrator,
    transcript: Transcript,
    builder: PromptBuilder,
    pcfg: PromptConfig,
    store: Option<MemoryStore>,
    ducker: Ducker,
    game: Option<GameCtx>,
    llm_handle: Option<tokio::task::JoinHandle<()>>,
    pending_question: Option<String>,
    toggle_held: bool,
    stt_warned: bool,

    pet_state: PetState,
    /// The pet is voicing a greeting or ambient remark (not a PTT answer).
    ambient_speaking: bool,
    last_ambient_ms: u64,
    last_ambient_hash: u64,
    last_interaction_ms: u64,
    sleeping: bool,
    compaction_inflight: bool,
    /// We auto-start 9router at most once per session.
    ninerouter_kicked: bool,
    greet_idx: usize,
}

pub fn run(deps: RunDeps) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let RunDeps { app, loaded, capture, mic, tts_tx, loop_tx, rx, status, muted } = deps;

    let store = match MemoryStore::open(
        &config_io::memory_db_path().to_string_lossy(),
        Box::new(HashEmbedder::default()),
    ) {
        Ok(s) => Some(s),
        Err(e) => {
            status.lock().unwrap().warnings.push(format!("memory store: {e}"));
            None
        }
    };

    let mut l = Loop {
        app,
        cfg: loaded.cfg,
        first_run: loaded.first_run,
        capture,
        mic,
        tts_tx,
        loop_tx,
        status,
        muted,
        client: reqwest::Client::new(),
        orch: Orchestrator::new(),
        transcript: Transcript::new(),
        builder: PromptBuilder::new(default_system_core()),
        pcfg: PromptConfig::default(),
        store,
        ducker: Ducker::default(),
        game: None,
        llm_handle: None,
        pending_question: None,
        toggle_held: false,
        stt_warned: false,
        pet_state: PetState::Watching,
        ambient_speaking: false,
        last_ambient_ms: 0,
        last_ambient_hash: 0,
        last_interaction_ms: now_ms(),
        sleeping: false,
        compaction_inflight: false,
        ninerouter_kicked: false,
        greet_idx: 0,
    };

    // Until a game is known, capture obeys the privacy default.
    l.capture.set_enabled(!l.cfg.privacy.game_foreground_only);
    l.emit_pet(PetState::Watching);
    l.refresh_status();

    loop {
        match rx.recv_timeout(Duration::from_millis(1000)) {
            Ok(msg) => l.handle(msg),
            Err(RecvTimeoutError::Timeout) => {
                l.refresh_status();
                l.maybe_sleep();
                l.maybe_ambient();
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

impl Loop {
    // ---- UI event mirror ---------------------------------------------------

    fn emit<T: Serialize + Clone>(&self, event: &str, payload: T) {
        let _ = self.app.emit(event, payload);
    }

    fn emit_pet(&mut self, s: PetState) {
        self.pet_state = s;
        let name = match s {
            PetState::Sleeping => "sleeping",
            PetState::Watching => "watching",
            PetState::Listening => "listening",
            PetState::Thinking => "thinking",
            PetState::Speaking => "speaking",
        };
        self.emit("rev:pet", name);
    }

    fn toast(&self, text: impl Into<String>) {
        self.emit("rev:toast", text.into());
    }

    fn transcript_line(&self, role: &str, text: &str) {
        self.emit("rev:transcript", serde_json::json!({ "role": role, "text": text }));
    }

    fn refresh_status(&self) {
        let mut st = self.status.lock().unwrap();
        st.game = self.game.as_ref().map(|g| g.name.clone());
        st.game_id = self.game.as_ref().map(|g| g.id);
        st.capture_enabled = self.capture.enabled.load(Ordering::Relaxed);
        st.capture_error = self.capture.error.lock().unwrap().clone();
        st.frames_seen = self.capture.frames_seen.load(Ordering::Relaxed);
        st.keyframes = self.capture.keyframes.load(Ordering::Relaxed);
        {
            let ring = self.capture.ring.lock().unwrap();
            st.ring_frames = ring.len();
            st.ring_kb = ring.byte_size() / 1024;
        }
        st.mic = self.mic.status.lock().unwrap().clone();
        st.qa_ready = config_io::qa_ready(&self.cfg);
        st.qa_kind = format!("{:?}", self.cfg.roles.qa.kind);
        st.qa_model = self.cfg.roles.qa.model.clone();
        st.stt_ready = config_io::stt_ready(&self.cfg);
        st.hotkey = self.cfg.hotkey.ptt.clone();
        st.config_path = config_io::config_path().to_string_lossy().to_string();
        st.muted = self.muted.load(Ordering::Relaxed);
        st.hype_mode = self.cfg.privacy.hype_mode;
        st.first_run = self.first_run;
        let snapshot = st.clone();
        drop(st);
        self.emit("rev:status", snapshot);
    }

    // ---- message dispatch --------------------------------------------------

    fn handle(&mut self, msg: LoopMsg) {
        match msg {
            LoopMsg::Input(ev) => {
                self.wake();
                // A PTT press while the pet is voicing a greeting/ambient
                // remark is a barge-in the orchestrator can't see (it's Idle).
                if matches!(ev, InputEvent::PttDown { .. }) && self.ambient_speaking {
                    let _ = self.tts_tx.send(TtsCmd::Stop);
                    self.ambient_speaking = false;
                }
                if matches!(ev, InputEvent::PlaybackFinished { .. }) && self.ambient_speaking {
                    self.ambient_speaking = false;
                    self.emit("rev:speech-done", ());
                    self.emit_pet(PetState::Watching);
                }
                if matches!(ev, InputEvent::Error { .. }) {
                    // A TTS failure during a greeting/remark must not wedge
                    // the ambient lane shut.
                    self.ambient_speaking = false;
                }
                self.handle_input(ev, true);
            }
            LoopMsg::Typed(q) => {
                self.wake();
                let q = q.trim().to_string();
                if q.is_empty() {
                    return;
                }
                let now = now_ms();
                self.handle_input(InputEvent::PttDown { ms: now }, false);
                self.handle_input(InputEvent::PttUp { ms: now }, false);
                self.handle_input(InputEvent::SttFinal { ms: now, text: q }, false);
            }
            LoopMsg::Delta(t) => self.emit("rev:delta", t),
            LoopMsg::ToolActivity(what) => self.emit("rev:tool", what),
            LoopMsg::LlmDone { full_text, usage, stop_reason } => {
                self.llm_handle = None;
                self.pending_question = None;
                if matches!(stop_reason.as_deref(), Some("length" | "max_tokens" | "MAX_TOKENS")) {
                    self.toast("Answer hit the token cap — raise max_output_tokens in config.toml");
                }
                if full_text.trim().is_empty() {
                    self.transcript.push_assistant("[empty reply]");
                    self.handle_input(
                        InputEvent::Error { ms: now_ms(), what: "empty reply".into() },
                        false,
                    );
                    return;
                }
                self.transcript.push_assistant(full_text.clone());
                self.transcript_line("assistant", &full_text);
                self.emit("rev:speech-done", ());
                if let Some(u) = usage {
                    self.record_usage(u);
                }
                self.maybe_compact();
            }
            LoopMsg::LlmFailed(what) => {
                self.llm_handle = None;
                if self.pending_question.take().is_some() {
                    self.transcript.push_assistant("[no answer — request failed]");
                }
                let friendly = self.map_llm_failure(&what);
                self.toast(friendly);
                self.handle_input(InputEvent::Error { ms: now_ms(), what }, false);
            }
            LoopMsg::SttFailed(what) => {
                self.toast(format!("Transcription failed: {what}"));
                self.handle_input(InputEvent::Error { ms: now_ms(), what }, false);
            }
            LoopMsg::GameChanged { exe_path, game } => self.on_game_changed(exe_path, game),
            LoopMsg::RememberNote(text) => self.on_remember(text, false),
            LoopMsg::ToolRemember(text) => self.on_remember(text, true),
            LoopMsg::ToolUpdateProfile(md) => self.on_update_profile(md),
            LoopMsg::EpisodeReady { game_id, started_epoch_ms, summary } => {
                if let Some(store) = &self.store {
                    let _ = store.add_episode(game_id, started_epoch_ms, epoch_ms(), &summary);
                }
            }
            LoopMsg::CompactionReady(summary) => {
                self.compaction_inflight = false;
                if self.transcript.needs_compaction(&self.pcfg) {
                    let folded = self.transcript.compact(&self.pcfg, &summary);
                    self.transcript_line("note", &format!("(compacted {folded} old turns)"));
                    // Splice indices shifted under the session slice.
                    if let Some(g) = self.game.as_mut() {
                        g.start_turn_idx = 0;
                    }
                }
            }
            LoopMsg::AmbientRemark(text) => self.deliver_ambient(text),
            LoopMsg::SetMuted(m) => {
                self.muted.store(m, Ordering::Relaxed);
                if m {
                    let _ = self.tts_tx.send(TtsCmd::Stop);
                    if self.ambient_speaking {
                        self.ambient_speaking = false;
                        self.emit_pet(PetState::Watching);
                    }
                }
                self.toast(if m { "Rev is muted." } else { "Rev can speak again." });
                self.refresh_status();
            }
            LoopMsg::Query(q) => self.answer_query(q),
            LoopMsg::Toast(text) => self.toast(text),
            LoopMsg::Shutdown => {
                self.flush_episode_blocking();
                self.app.exit(0);
            }
        }
    }

    /// `from_hook` applies hold-vs-toggle translation; synthetic events skip it.
    fn handle_input(&mut self, ev: InputEvent, from_hook: bool) {
        let ev = if from_hook && self.cfg.hotkey.toggle_mode {
            match ev {
                InputEvent::PttDown { ms } => {
                    if !self.toggle_held {
                        self.toggle_held = true;
                        InputEvent::PttDown { ms }
                    } else {
                        self.toggle_held = false;
                        InputEvent::PttUp { ms }
                    }
                }
                InputEvent::PttUp { .. } => return, // tap mode ignores releases
                other => other,
            }
        } else {
            ev
        };

        let is_first_audio = matches!(ev, InputEvent::TtsFirstAudio { .. });
        let actions = self.orch.handle(ev);
        for a in actions {
            self.exec(a);
        }
        if is_first_audio && !self.ambient_speaking {
            if let Some(r) = self.orch.marks.report() {
                self.emit(
                    "rev:latency",
                    serde_json::json!({
                        "total_ms": r.release_to_first_audio_ms,
                        "stt_ms": r.stt_ms,
                        "llm_ttft_ms": r.llm_ttft_ms,
                        "tts_ms": r.tts_ms,
                        "within_budget": r.within_budget,
                    }),
                );
            }
        }
    }

    // ---- action execution --------------------------------------------------

    fn exec(&mut self, action: Action) {
        match action {
            Action::SetPet(s) => self.emit_pet(s),
            Action::DuckGameAudio(on) => self.ducker.set(on),
            Action::CaptureFreshFrame => {
                self.capture
                    .fresh_request
                    .store(true, Ordering::Relaxed);
            }
            Action::StartSttStream => self.mic.start(),
            Action::FinalizeStt => self.finalize_stt(),
            Action::StopPlaybackAndCancel => {
                if let Some(h) = self.llm_handle.take() {
                    h.abort();
                }
                if self.pending_question.take().is_some() {
                    self.transcript.push_assistant("[interrupted]");
                }
                let _ = self.tts_tx.send(TtsCmd::Stop);
                self.emit("rev:speech-done", ());
            }
            Action::SendChatRequest { question } => self.send_chat(question),
        }
    }

    fn finalize_stt(&mut self) {
        let wav = self.mic.stop_take_wav();
        let stt_cfg = self.cfg.voice.stt.clone().filter(|_| config_io::stt_ready(&self.cfg));
        match (wav, stt_cfg) {
            (Some(wav), Some(cfg)) => {
                let tx = self.loop_tx.clone();
                let client = self.client.clone();
                let game = self.game.as_ref().map(|g| g.name.clone());
                llm::rt().spawn(async move {
                    match stt::transcribe(&client, &cfg, wav, game.as_deref()).await {
                        Ok(text) => {
                            if text.is_empty() {
                                // Nothing intelligible (or the defection guard
                                // discarded an assistant-shaped result).
                                let _ = tx.send(LoopMsg::Toast(
                                    "Didn't catch that — hold the key and try again.".into(),
                                ));
                            }
                            let _ = tx.send(LoopMsg::Input(InputEvent::SttFinal {
                                ms: now_ms(),
                                text,
                            }));
                        }
                        Err(e) => {
                            let _ = tx.send(LoopMsg::SttFailed(format!("{e:#}")));
                        }
                    }
                });
            }
            (_, None) => {
                if !self.stt_warned {
                    self.stt_warned = true;
                    self.toast(
                        "No STT configured — click the pet and type, or add [voice.stt] to config.toml",
                    );
                }
                let _ = self
                    .loop_tx
                    .send(LoopMsg::Input(InputEvent::SttFinal { ms: now_ms(), text: String::new() }));
            }
            (None, Some(_)) => {
                // Under ~250 ms of audio — treat as an empty ask.
                let _ = self
                    .loop_tx
                    .send(LoopMsg::Input(InputEvent::SttFinal { ms: now_ms(), text: String::new() }));
            }
        }
    }

    /// Frames for a question, with two guarantees the ring alone can't give:
    /// (1) the fresh frame requested at PTT-down gets a bounded wait to
    /// actually land (typed asks fire PTT-down and the question in the same
    /// millisecond — without the wait they'd ship a pre-press ring state);
    /// (2) if capture is paused because no game was detected, an explicit
    /// ask runs a one-shot grab — asking IS the consent `upload_on_ask_only`
    /// is about — and the privacy pause is restored right after.
    fn collect_frames(&mut self) -> Vec<ImageAttachment> {
        let capture_dead = self.capture.error.lock().unwrap().is_some();
        let was_enabled = self.capture.enabled.load(Ordering::Relaxed);
        if !was_enabled && !capture_dead {
            self.capture.set_enabled(true);
            self.capture.fresh_request.store(true, Ordering::Relaxed);
        }
        if !capture_dead {
            let asked = self.orch.marks.ptt_down.unwrap_or_else(now_ms);
            let deadline = now_ms() + if was_enabled { 500 } else { 900 };
            while self.capture.last_ms.load(Ordering::Relaxed) < asked && now_ms() < deadline {
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        let frames: Vec<ImageAttachment> = {
            let ring = self.capture.ring.lock().unwrap();
            ring.select_for_question(self.pcfg.max_images_per_question, 5_000)
                .into_iter()
                .map(|kf| ImageAttachment {
                    media_type: "image/jpeg".into(),
                    base64_data: base64::engine::general_purpose::STANDARD.encode(&kf.jpeg),
                })
                .collect()
        };
        if !was_enabled && !capture_dead {
            // Restore the privacy pause (this wipes the ring again).
            self.capture.set_enabled(false);
        }
        frames
    }

    fn send_chat(&mut self, question: String) {
        if !config_io::qa_ready(&self.cfg) {
            self.toast("No API key yet — click the pet and finish setup");
            self.handle_input(
                InputEvent::Error { ms: now_ms(), what: "qa key missing".into() },
                false,
            );
            return;
        }
        // Monthly budget gate — only enforceable when prices are configured.
        if let Some(store) = &self.store {
            if let Ok(m) = store.month_usage(&month_string()) {
                if self.cfg.budget.over_cap(m.input_tokens, m.output_tokens) {
                    self.toast(format!(
                        "Monthly budget cap (${:.2}) reached — raise it in Settings to keep asking.",
                        self.cfg.budget.monthly_usd_cap
                    ));
                    self.handle_input(
                        InputEvent::Error { ms: now_ms(), what: "budget cap reached".into() },
                        false,
                    );
                    return;
                }
            }
        }

        // Frames: freshest + highest-change, capped by the prompt config.
        let frames = self.collect_frames();

        let memories: Vec<String> = match (&self.store, &self.game) {
            (Some(store), Some(game)) => store
                .retrieve(game.id, &question, 3)
                .map(|facts| facts.into_iter().map(|f| f.text).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };

        let profile = self
            .game
            .as_ref()
            .map(|g| g.profile.clone())
            .unwrap_or_else(|| "New player — no profile yet.".to_string());
        let episode = self.game.as_ref().and_then(|g| g.episode.clone());

        let today = crate::util::local_date_string();
        let req = self.builder.build(
            &self.pcfg,
            Some(&today),
            &profile,
            episode.as_deref(),
            &memories,
            &self.transcript,
            &question,
            frames,
        );

        self.transcript.push_user(question.clone(), Vec::new());
        self.transcript_line("user", &question);
        self.pending_question = Some(question);
        if let Some(g) = self.game.as_mut() {
            g.asks += 1;
        }

        self.llm_handle = Some(llm::spawn_stream(
            self.client.clone(),
            self.cfg.roles.qa.clone(),
            self.cfg.roles.search.clone(),
            req,
            self.loop_tx.clone(),
            self.tts_tx.clone(),
        ));
    }

    fn record_usage(&mut self, u: Usage) {
        self.status.lock().unwrap().last_usage = Some(UsageJson {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
        });
        if let Some(store) = &self.store {
            let _ = store.add_usage(&month_string(), u.input_tokens, u.output_tokens);
        }
    }

    /// Translate raw transport errors into something a player can act on,
    /// and auto-start 9router once when the local brain endpoint is down.
    fn map_llm_failure(&mut self, what: &str) -> String {
        let base = self.cfg.roles.qa.base_url.clone().unwrap_or_default();
        let local = base.contains("localhost") || base.contains("127.0.0.1");
        let lower = what.to_lowercase();
        let conn_down = lower.contains("error sending request")
            || lower.contains("connection refused")
            || lower.contains("10061")
            || lower.contains("connect");
        if local && conn_down {
            if !self.ninerouter_kicked && base.contains(":20128") {
                self.ninerouter_kicked = true;
                crate::util::launch_9router();
                return "Your local brain router wasn't running — I just started it. \
                        Ask me again in about five seconds."
                    .into();
            }
            return "The local brain router isn't reachable yet — give it a few seconds \
                    and ask again."
                .into();
        }
        if lower.contains("401") || lower.contains("unauthorized") || lower.contains("invalid api key") {
            return "The model rejected your API key — check it in the panel's Settings tab.".into();
        }
        if lower.contains("429") || lower.contains("rate limit") {
            return "The provider is rate-limiting — give it a moment and try again.".into();
        }
        let short: String = what.chars().take(160).collect();
        format!("Model request failed: {short}")
    }

    // ---- game + memory -----------------------------------------------------

    fn on_game_changed(&mut self, exe_path: String, game: Option<(String, String)>) {
        self.status.lock().unwrap().foreground_exe =
            exe_path.rsplit(['\\', '/']).next().unwrap_or("").to_string();
        match game {
            Some((name, exe)) => {
                if self.game.as_ref().map(|g| g.name.as_str()) == Some(name.as_str()) {
                    // Back in the same game after an alt-tab. The None arm
                    // paused capture on the way out, so re-arm it here —
                    // early-returning without this left the pet permanently
                    // blind for the rest of the session.
                    if !self.capture.enabled.load(Ordering::Relaxed) {
                        self.capture.set_enabled(true);
                        self.refresh_status();
                    }
                    return;
                }
                self.wake();
                // Different game: summarize the outgoing session first.
                self.flush_episode_async();
                let ctx = self.store.as_ref().and_then(|store| {
                    let id = store.get_or_create_game(&name, Some(&exe)).ok()?;
                    let profile = store
                        .get_profile(id)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "New player — no profile yet.".to_string());
                    let episode = store.latest_episode(id).ok().flatten();
                    Some(GameCtx {
                        id,
                        name: name.clone(),
                        profile,
                        episode,
                        session_start_epoch_ms: epoch_ms(),
                        start_turn_idx: self.transcript.turns().len(),
                        asks: 0,
                    })
                });
                self.game = ctx;
                self.capture.set_enabled(true);
                self.transcript.push_note(format!("Game switched to {name}."));
                self.toast(format!("Watching {name}"));
                self.greet(&name);
                self.refresh_status();
            }
            None => {
                // Foreground is not a known game.
                if self.cfg.privacy.game_foreground_only {
                    self.capture.set_enabled(false);
                }
                self.refresh_status();
            }
        }
    }

    fn on_remember(&mut self, text: String, from_tool: bool) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let current_game = self.game.as_ref().map(|g| g.id);
        // Compute first so the store borrow ends before any UI call.
        let outcome: Result<FactGateDecision, String> = match self.store.as_mut() {
            None => Err("Memory store unavailable".into()),
            Some(store) => {
                // Fall back to a catch-all profile when no game is active.
                let game_id = match current_game {
                    Some(id) => Ok(id),
                    None => store
                        .get_or_create_game("Desktop", None)
                        .map_err(|e| e.to_string()),
                };
                game_id.and_then(|id| {
                    store
                        .remember(id, &text, "note", epoch_ms())
                        .map_err(|e| e.to_string())
                })
            }
        };
        match outcome {
            Ok(FactGateDecision::Add) => {
                self.toast(if from_tool { "Rev saved a memory." } else { "Remembered." })
            }
            Ok(FactGateDecision::Update(_)) => self.toast("Updated an existing memory."),
            Ok(FactGateDecision::Noop) => {
                if !from_tool {
                    self.toast("Already knew that.")
                }
            }
            Err(e) => self.toast(format!("Could not save: {e}")),
        }
    }

    fn on_update_profile(&mut self, markdown: String) {
        let markdown = markdown.trim().to_string();
        if markdown.is_empty() {
            return;
        }
        let Some(g) = self.game.as_ref() else {
            return;
        };
        let (id, ok) = match &self.store {
            Some(store) => (g.id, store.set_profile(g.id, &markdown, epoch_ms()).is_ok()),
            None => (g.id, false),
        };
        if ok {
            if let Some(g) = self.game.as_mut() {
                if g.id == id {
                    g.profile = markdown;
                }
            }
            self.toast("Rev updated your player profile.");
        }
    }

    fn answer_query(&mut self, q: UiQuery) {
        match q {
            UiQuery::Games(reply) => {
                let rows = self
                    .store
                    .as_ref()
                    .and_then(|s| s.list_games().ok())
                    .unwrap_or_default();
                let _ = reply.send(rows);
            }
            UiQuery::Facts { game_id, reply } => {
                let rows = self
                    .store
                    .as_ref()
                    .and_then(|s| s.list_facts(game_id).ok())
                    .unwrap_or_default();
                let _ = reply.send(rows);
            }
            UiQuery::Forget { fact_id, reply } => {
                let ok = self
                    .store
                    .as_ref()
                    .and_then(|s| s.delete_fact(fact_id).ok())
                    .unwrap_or(false);
                let _ = reply.send(ok);
            }
            UiQuery::Profile { game_id, reply } => {
                let md = self
                    .store
                    .as_ref()
                    .and_then(|s| s.get_profile(game_id).ok())
                    .flatten()
                    .unwrap_or_default();
                let _ = reply.send(md);
            }
            UiQuery::SetProfile { game_id, markdown, reply } => {
                let ok = self
                    .store
                    .as_ref()
                    .map(|s| s.set_profile(game_id, &markdown, epoch_ms()).is_ok())
                    .unwrap_or(false);
                if ok {
                    if let Some(g) = self.game.as_mut() {
                        if g.id == game_id {
                            g.profile = markdown;
                        }
                    }
                }
                let _ = reply.send(ok);
            }
            UiQuery::Usage(reply) => {
                let month = month_string();
                let m = self
                    .store
                    .as_ref()
                    .and_then(|s| s.month_usage(&month).ok())
                    .unwrap_or_default();
                let _ = reply.send(UsageSnapshot::from_store(month, m, &self.cfg.budget));
            }
        }
    }

    // ---- session episodes --------------------------------------------------

    /// The current game session's dialogue as plain text (empty if trivial).
    fn session_text(&self) -> Option<(i64, u64, String)> {
        let g = self.game.as_ref()?;
        if g.asks == 0 {
            return None;
        }
        let turns = self.transcript.turns();
        let slice = &turns[g.start_turn_idx.min(turns.len())..];
        let mut text = String::new();
        for t in slice {
            match t {
                Turn::User { text: q, .. } => text.push_str(&format!("Player: {q}\n")),
                Turn::Assistant { text: a } => text.push_str(&format!("Rev: {a}\n")),
                Turn::SystemNote { .. } => {}
            }
        }
        (!text.trim().is_empty()).then(|| (g.id, g.session_start_epoch_ms, text))
    }

    /// Summarize the outgoing session in the background (game switch).
    fn flush_episode_async(&mut self) {
        if !config_io::qa_ready(&self.cfg) {
            return;
        }
        if let Some((game_id, started_epoch_ms, text)) = self.session_text() {
            llm::spawn_summarize(
                self.client.clone(),
                self.cfg.roles.qa.clone(),
                text,
                SummarizeTarget::Episode { game_id, started_epoch_ms },
                self.loop_tx.clone(),
            );
        }
    }

    /// Summarize synchronously with a hard deadline (quit path).
    fn flush_episode_blocking(&mut self) {
        if !config_io::qa_ready(&self.cfg) {
            return;
        }
        if let Some((game_id, started_epoch_ms, text)) = self.session_text() {
            if let Some(summary) = llm::summarize_blocking(
                &self.client,
                &self.cfg.roles.qa,
                &text,
                Duration::from_secs(6),
            ) {
                if let Some(store) = &self.store {
                    let _ = store.add_episode(game_id, started_epoch_ms, epoch_ms(), &summary);
                }
            }
        }
    }

    fn maybe_compact(&mut self) {
        if self.compaction_inflight || !self.transcript.needs_compaction(&self.pcfg) {
            return;
        }
        // Heuristic fallback: the model-written summary is preferred, but
        // compaction must complete even offline.
        let fallback: String = self
            .transcript
            .turns()
            .iter()
            .filter_map(|t| match t {
                Turn::User { text, .. } => Some(text.chars().take(60).collect::<String>()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("; ");
        if !config_io::qa_ready(&self.cfg) {
            let folded = self.transcript.compact(&self.pcfg, &fallback);
            self.transcript_line("note", &format!("(compacted {folded} old turns)"));
            if let Some(g) = self.game.as_mut() {
                g.start_turn_idx = 0;
            }
            return;
        }
        let mut text = String::new();
        for t in self.transcript.turns() {
            match t {
                Turn::User { text: q, .. } => text.push_str(&format!("Player: {q}\n")),
                Turn::Assistant { text: a } => text.push_str(&format!("Rev: {a}\n")),
                Turn::SystemNote { .. } => {}
            }
        }
        self.compaction_inflight = true;
        llm::spawn_summarize(
            self.client.clone(),
            self.cfg.roles.qa.clone(),
            text,
            SummarizeTarget::Compaction { fallback },
            self.loop_tx.clone(),
        );
    }

    // ---- companion behaviors ----------------------------------------------

    fn wake(&mut self) {
        self.last_interaction_ms = now_ms();
        if self.sleeping {
            self.sleeping = false;
            self.emit_pet(PetState::Watching);
        }
    }

    /// With no game and no interaction for a while, the pet dozes off —
    /// purely cosmetic, and any event wakes it.
    fn maybe_sleep(&mut self) {
        let after = self.cfg.companion.sleep_after_secs;
        if after == 0 || self.sleeping || self.game.is_some() {
            return;
        }
        if self.pet_state == PetState::Watching
            && now_ms().saturating_sub(self.last_interaction_ms) > after * 1000
        {
            self.sleeping = true;
            self.emit_pet(PetState::Sleeping);
        }
    }

    /// Short spoken hello when a game comes into focus.
    fn greet(&mut self, game_name: &str) {
        if !self.cfg.companion.greet_on_game || self.muted.load(Ordering::Relaxed) {
            return;
        }
        let line = GREETS[self.greet_idx % GREETS.len()].replace("{}", game_name);
        self.greet_idx += 1;
        self.emit("rev:pet", "celebrate");
        self.emit("rev:delta", line.clone());
        self.ambient_speaking = true;
        let _ = self.tts_tx.send(TtsCmd::Sentence(line));
        let _ = self.tts_tx.send(TtsCmd::EndOfUtterance);
    }

    /// The opt-in hype lane: an occasional one-liner in natural pauses.
    /// Hard conditions keep it from ever talking over a fight or an answer.
    fn maybe_ambient(&mut self) {
        if !self.cfg.privacy.hype_mode
            || self.muted.load(Ordering::Relaxed)
            || self.ambient_speaking
            || self.llm_handle.is_some()
            || self.pet_state != PetState::Watching
            || !config_io::qa_ready(&self.cfg)
        {
            return;
        }
        let Some(game_name) = self.game.as_ref().map(|g| g.name.clone()) else {
            return;
        };
        let gap_ms = self.cfg.companion.ambient_min_gap_secs.max(30) * 1000;
        let now = now_ms();
        if now.saturating_sub(self.last_ambient_ms) < gap_ms {
            return;
        }
        // Budget gate applies to ambient too.
        if let Some(store) = &self.store {
            if let Ok(m) = store.month_usage(&month_string()) {
                if self.cfg.budget.over_cap(m.input_tokens, m.output_tokens) {
                    return;
                }
            }
        }
        // Only remark on a frame we haven't remarked on (something happened),
        // and only when it's fresh.
        let frame = {
            let ring = self.capture.ring.lock().unwrap();
            ring.latest().map(|kf| {
                (
                    kf.hash,
                    kf.ts_ms,
                    ImageAttachment {
                        media_type: "image/jpeg".into(),
                        base64_data: base64::engine::general_purpose::STANDARD.encode(&kf.jpeg),
                    },
                )
            })
        };
        let Some((hash, ts, frame)) = frame else { return };
        if hash == self.last_ambient_hash || now.saturating_sub(ts) > 30_000 {
            return;
        }
        self.last_ambient_ms = now;
        self.last_ambient_hash = hash;

        let mut cfg = self
            .cfg
            .roles
            .ambient
            .clone()
            .unwrap_or_else(|| self.cfg.roles.qa.clone());
        cfg.max_output_tokens = 80;
        cfg.enable_web_search = false;
        let req = ChatRequest {
            system: ambient_system_core(),
            context_block: format!("## Game\n{game_name}\n"),
            turns: vec![Turn::User {
                text: "Latest frame attached. One remark if the moment deserves it, \
                       or reply with exactly PASS."
                    .into(),
                images: vec![frame],
            }],
        };
        let client = self.client.clone();
        let tx = self.loop_tx.clone();
        llm::rt().spawn(async move {
            if let Ok(text) = llm::collect_stream(&client, &cfg, &req).await {
                let t = text.trim();
                if !t.is_empty() && !t.eq_ignore_ascii_case("pass") && t.len() < 300 {
                    let _ = tx.send(LoopMsg::AmbientRemark(t.to_string()));
                }
            }
        });
    }

    /// An ambient remark arrived — say it only if the moment is still quiet.
    fn deliver_ambient(&mut self, text: String) {
        if self.muted.load(Ordering::Relaxed)
            || self.ambient_speaking
            || self.llm_handle.is_some()
            || self.pet_state != PetState::Watching
        {
            return;
        }
        let spoken = llm::speakable(&text);
        if spoken.trim().is_empty() {
            return;
        }
        self.transcript.push_assistant(text.clone());
        self.transcript_line("assistant", &text);
        self.emit("rev:delta", text);
        self.emit_pet(PetState::Speaking);
        self.ambient_speaking = true;
        let _ = self.tts_tx.send(TtsCmd::Sentence(spoken));
        let _ = self.tts_tx.send(TtsCmd::EndOfUtterance);
    }
}
