//! The orchestrator run-loop: the single thread that owns the core state
//! machine, the transcript, and the memory store, executes the `Action`s the
//! orchestrator returns, and mirrors everything to the UI via Tauri events.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use revolution_core::config::AppConfig;
use revolution_core::memory::embed::HashEmbedder;
use revolution_core::memory::store::{FactGateDecision, MemoryStore};
use revolution_core::orchestrator::{Action, InputEvent, Orchestrator, PetState};
use revolution_core::prompt::{default_system_core, PromptBuilder, PromptConfig, Transcript};
use revolution_core::providers::ImageAttachment;
use serde::Serialize;
use tauri::Emitter;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::capture::CaptureShared;
use crate::config_io::{self, LoadedConfig};
use crate::duck::Ducker;
use crate::llm;
use crate::mic::MicHandle;
use crate::msg::{LoopMsg, TtsCmd};
use crate::stt;
use crate::util::now_ms;

/// Panel-facing status snapshot (also emitted as `rev:status`).
#[derive(Clone, Default, Serialize)]
pub struct Status {
    pub game: Option<String>,
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
}

struct GameCtx {
    id: i64,
    name: String,
    profile: String,
    episode: Option<String>,
}

struct Loop {
    app: tauri::AppHandle,
    cfg: AppConfig,
    capture: Arc<CaptureShared>,
    mic: MicHandle,
    tts_tx: Sender<TtsCmd>,
    loop_tx: Sender<LoopMsg>,
    status: Arc<Mutex<Status>>,
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
}

pub fn run(deps: RunDeps) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let RunDeps { app, loaded, capture, mic, tts_tx, loop_tx, rx, status } = deps;

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
        capture,
        mic,
        tts_tx,
        loop_tx,
        status,
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
    };

    // Until a game is known, capture obeys the privacy default.
    l.capture.set_enabled(!l.cfg.privacy.game_foreground_only);
    l.emit_pet(PetState::Watching);
    l.refresh_status();

    loop {
        match rx.recv_timeout(Duration::from_millis(1000)) {
            Ok(msg) => l.handle(msg),
            Err(RecvTimeoutError::Timeout) => l.refresh_status(),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

impl Loop {
    // ---- UI event mirror ---------------------------------------------------

    fn emit<T: Serialize + Clone>(&self, event: &str, payload: T) {
        let _ = self.app.emit(event, payload);
    }

    fn emit_pet(&self, s: PetState) {
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
        st.capture_enabled = self.capture.enabled.load(std::sync::atomic::Ordering::Relaxed);
        st.capture_error = self.capture.error.lock().unwrap().clone();
        st.frames_seen = self.capture.frames_seen.load(std::sync::atomic::Ordering::Relaxed);
        st.keyframes = self.capture.keyframes.load(std::sync::atomic::Ordering::Relaxed);
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
        let snapshot = st.clone();
        drop(st);
        self.emit("rev:status", snapshot);
    }

    // ---- message dispatch --------------------------------------------------

    fn handle(&mut self, msg: LoopMsg) {
        match msg {
            LoopMsg::Input(ev) => self.handle_input(ev, true),
            LoopMsg::Typed(q) => {
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
                    self.status.lock().unwrap().last_usage = Some(UsageJson {
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        cache_read_tokens: u.cache_read_tokens,
                    });
                }
                self.maybe_compact();
            }
            LoopMsg::LlmFailed(what) => {
                self.llm_handle = None;
                if self.pending_question.take().is_some() {
                    self.transcript.push_assistant("[no answer — request failed]");
                }
                self.toast(format!("Model request failed: {what}"));
                self.handle_input(InputEvent::Error { ms: now_ms(), what }, false);
            }
            LoopMsg::SttFailed(what) => {
                self.toast(format!("Transcription failed: {what}"));
                self.handle_input(InputEvent::Error { ms: now_ms(), what }, false);
            }
            LoopMsg::GameChanged { exe_path, game } => self.on_game_changed(exe_path, game),
            LoopMsg::RememberNote(text) => self.on_remember(text),
            LoopMsg::Toast(text) => self.toast(text),
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
        if is_first_audio {
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
                    .store(true, std::sync::atomic::Ordering::Relaxed);
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
        use std::sync::atomic::Ordering;
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
            self.toast("No API key yet — open the panel and add your roles.qa key");
            self.handle_input(
                InputEvent::Error { ms: now_ms(), what: "qa key missing".into() },
                false,
            );
            return;
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

        self.llm_handle = Some(llm::spawn_stream(
            self.client.clone(),
            self.cfg.roles.qa.clone(),
            self.cfg.roles.search.clone(),
            req,
            self.loop_tx.clone(),
            self.tts_tx.clone(),
        ));
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
                    if !self.capture.enabled.load(std::sync::atomic::Ordering::Relaxed) {
                        self.capture.set_enabled(true);
                        self.refresh_status();
                    }
                    return;
                }
                let ctx = self.store.as_ref().and_then(|store| {
                    let id = store.get_or_create_game(&name, Some(&exe)).ok()?;
                    let profile = store
                        .get_profile(id)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "New player — no profile yet.".to_string());
                    let episode = store.latest_episode(id).ok().flatten();
                    Some(GameCtx { id, name: name.clone(), profile, episode })
                });
                self.game = ctx;
                self.capture.set_enabled(true);
                self.transcript.push_note(format!("Game switched to {name}."));
                self.toast(format!("Watching {name}"));
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

    fn on_remember(&mut self, text: String) {
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
                        .remember(id, &text, "note", now_ms())
                        .map_err(|e| e.to_string())
                })
            }
        };
        match outcome {
            Ok(FactGateDecision::Add) => self.toast("Remembered."),
            Ok(FactGateDecision::Update(_)) => self.toast("Updated an existing memory."),
            Ok(FactGateDecision::Noop) => self.toast("Already knew that."),
            Err(e) => self.toast(format!("Could not save: {e}")),
        }
    }

    fn maybe_compact(&mut self) {
        if !self.transcript.needs_compaction(&self.pcfg) {
            return;
        }
        // v0 heuristic summary: the model-written summarizer is Phase 2.
        let summary: String = self
            .transcript
            .turns()
            .iter()
            .filter_map(|t| match t {
                revolution_core::providers::Turn::User { text, .. } => {
                    Some(text.chars().take(60).collect::<String>())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("; ");
        let folded = self.transcript.compact(&self.pcfg, &summary);
        self.transcript_line("note", &format!("(compacted {folded} old turns)"));
    }
}
