//! Push-to-talk orchestrator (DESIGN.md §3.2, §4): a pure state machine.
//!
//! The shell feeds it input events (hotkey, STT, stream, audio callbacks)
//! with millisecond timestamps; it returns the actions to perform. Keeping
//! it sans-IO makes the whole interaction model — including barge-in and the
//! latency ledger — testable without a single real device.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PetState {
    Sleeping,
    Watching,
    Listening,
    Thinking,
    Speaking,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    PttDown { ms: u64 },
    PttUp { ms: u64 },
    SttFinal { ms: u64, text: String },
    FirstToken { ms: u64 },
    TtsFirstAudio { ms: u64 },
    PlaybackFinished { ms: u64 },
    Error { ms: u64, what: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Capture + encode the freshest frame NOW (key-down; off the critical path).
    CaptureFreshFrame,
    StartSttStream,
    FinalizeStt,
    DuckGameAudio(bool),
    /// Barge-in: stop playback, flush TTS queue, cancel in-flight LLM stream.
    StopPlaybackAndCancel,
    SendChatRequest { question: String },
    SetPet(PetState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TalkState {
    Idle,
    Listening,
    AwaitingStt,
    Answering,
    Speaking,
}

/// The voice-to-voice budget the product promises (DESIGN.md §4).
pub const VOICE_BUDGET_MS: u64 = 2_000;

#[derive(Debug, Default, Clone)]
pub struct LatencyMarks {
    pub ptt_down: Option<u64>,
    pub ptt_up: Option<u64>,
    pub stt_final: Option<u64>,
    pub first_token: Option<u64>,
    pub first_audio: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LatencyReport {
    pub release_to_first_audio_ms: u64,
    pub stt_ms: u64,
    pub llm_ttft_ms: u64,
    pub tts_ms: u64,
    pub within_budget: bool,
}

impl LatencyMarks {
    pub fn report(&self) -> Option<LatencyReport> {
        let up = self.ptt_up?;
        let stt = self.stt_final?;
        let tok = self.first_token?;
        let audio = self.first_audio?;
        let total = audio.saturating_sub(up);
        Some(LatencyReport {
            release_to_first_audio_ms: total,
            stt_ms: stt.saturating_sub(up),
            llm_ttft_ms: tok.saturating_sub(stt),
            tts_ms: audio.saturating_sub(tok),
            within_budget: total <= VOICE_BUDGET_MS,
        })
    }
}

pub struct Orchestrator {
    state: TalkState,
    pub marks: LatencyMarks,
}

impl Default for Orchestrator {
    fn default() -> Self {
        Self::new()
    }
}

impl Orchestrator {
    pub fn new() -> Self {
        Self { state: TalkState::Idle, marks: LatencyMarks::default() }
    }

    fn begin_listening(&mut self, ms: u64, actions: &mut Vec<Action>) {
        self.state = TalkState::Listening;
        self.marks = LatencyMarks { ptt_down: Some(ms), ..Default::default() };
        actions.extend([
            Action::SetPet(PetState::Listening),
            Action::DuckGameAudio(true),
            Action::CaptureFreshFrame,
            Action::StartSttStream,
        ]);
    }

    pub fn handle(&mut self, ev: InputEvent) -> Vec<Action> {
        let mut actions = Vec::new();
        match (self.state, ev) {
            (TalkState::Idle, InputEvent::PttDown { ms }) => {
                self.begin_listening(ms, &mut actions);
            }
            // Barge-in: interrupt the pet mid-sentence, instantly.
            (TalkState::Speaking, InputEvent::PttDown { ms })
            | (TalkState::Answering, InputEvent::PttDown { ms }) => {
                actions.push(Action::StopPlaybackAndCancel);
                self.begin_listening(ms, &mut actions);
            }
            (TalkState::Listening, InputEvent::PttUp { ms }) => {
                self.state = TalkState::AwaitingStt;
                self.marks.ptt_up = Some(ms);
                actions.extend([Action::FinalizeStt, Action::SetPet(PetState::Thinking)]);
            }
            (TalkState::AwaitingStt, InputEvent::SttFinal { ms, text }) => {
                self.marks.stt_final = Some(ms);
                if text.trim().is_empty() {
                    // Nothing intelligible — go back to watching quietly.
                    self.state = TalkState::Idle;
                    actions.extend([
                        Action::SetPet(PetState::Watching),
                        Action::DuckGameAudio(false),
                    ]);
                } else {
                    self.state = TalkState::Answering;
                    actions.push(Action::SendChatRequest { question: text });
                }
            }
            (TalkState::Answering, InputEvent::FirstToken { ms }) => {
                self.marks.first_token = Some(ms);
            }
            (TalkState::Answering, InputEvent::TtsFirstAudio { ms }) => {
                self.marks.first_audio = Some(ms);
                self.state = TalkState::Speaking;
                actions.push(Action::SetPet(PetState::Speaking));
            }
            (TalkState::Speaking, InputEvent::PlaybackFinished { .. }) => {
                self.state = TalkState::Idle;
                actions.extend([
                    Action::SetPet(PetState::Watching),
                    Action::DuckGameAudio(false),
                ]);
            }
            (_, InputEvent::Error { .. }) => {
                self.state = TalkState::Idle;
                actions.extend([
                    Action::StopPlaybackAndCancel,
                    Action::SetPet(PetState::Watching),
                    Action::DuckGameAudio(false),
                ]);
            }
            _ => {}
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_produces_actions_and_budget_report() {
        let mut o = Orchestrator::new();

        let a = o.handle(InputEvent::PttDown { ms: 10_000 });
        assert_eq!(
            a,
            vec![
                Action::SetPet(PetState::Listening),
                Action::DuckGameAudio(true),
                Action::CaptureFreshFrame,
                Action::StartSttStream,
            ]
        );

        let a = o.handle(InputEvent::PttUp { ms: 13_000 });
        assert_eq!(a[0], Action::FinalizeStt);

        let a = o.handle(InputEvent::SttFinal { ms: 13_250, text: "best pal for mining?".into() });
        assert_eq!(a, vec![Action::SendChatRequest { question: "best pal for mining?".into() }]);

        assert!(o.handle(InputEvent::FirstToken { ms: 13_900 }).is_empty());
        let a = o.handle(InputEvent::TtsFirstAudio { ms: 14_200 });
        assert_eq!(a, vec![Action::SetPet(PetState::Speaking)]);

        let r = o.marks.report().expect("complete marks");
        assert_eq!(r.release_to_first_audio_ms, 1_200);
        assert_eq!(r.stt_ms, 250);
        assert_eq!(r.llm_ttft_ms, 650);
        assert_eq!(r.tts_ms, 300);
        assert!(r.within_budget);

        let a = o.handle(InputEvent::PlaybackFinished { ms: 20_000 });
        assert_eq!(a, vec![Action::SetPet(PetState::Watching), Action::DuckGameAudio(false)]);
    }

    #[test]
    fn barge_in_while_speaking_cancels_and_relistens() {
        let mut o = Orchestrator::new();
        o.handle(InputEvent::PttDown { ms: 0 });
        o.handle(InputEvent::PttUp { ms: 1_000 });
        o.handle(InputEvent::SttFinal { ms: 1_200, text: "hi".into() });
        o.handle(InputEvent::TtsFirstAudio { ms: 2_000 });

        let a = o.handle(InputEvent::PttDown { ms: 3_000 });
        assert_eq!(a[0], Action::StopPlaybackAndCancel);
        assert!(a.contains(&Action::StartSttStream));
        // Marks were reset for the new turn.
        assert_eq!(o.marks.ptt_down, Some(3_000));
        assert_eq!(o.marks.first_audio, None);
    }

    #[test]
    fn empty_transcript_returns_to_watching() {
        let mut o = Orchestrator::new();
        o.handle(InputEvent::PttDown { ms: 0 });
        o.handle(InputEvent::PttUp { ms: 500 });
        let a = o.handle(InputEvent::SttFinal { ms: 700, text: "   ".into() });
        assert_eq!(a, vec![Action::SetPet(PetState::Watching), Action::DuckGameAudio(false)]);
    }

    #[test]
    fn over_budget_is_reported() {
        let mut o = Orchestrator::new();
        o.handle(InputEvent::PttDown { ms: 0 });
        o.handle(InputEvent::PttUp { ms: 1_000 });
        o.handle(InputEvent::SttFinal { ms: 1_400, text: "q".into() });
        o.handle(InputEvent::FirstToken { ms: 3_000 });
        o.handle(InputEvent::TtsFirstAudio { ms: 3_600 });
        let r = o.marks.report().unwrap();
        assert_eq!(r.release_to_first_audio_ms, 2_600);
        assert!(!r.within_budget);
    }

    #[test]
    fn errors_always_land_back_in_watching() {
        let mut o = Orchestrator::new();
        o.handle(InputEvent::PttDown { ms: 0 });
        let a = o.handle(InputEvent::Error { ms: 100, what: "stt socket died".into() });
        assert!(a.contains(&Action::SetPet(PetState::Watching)));
        assert!(a.contains(&Action::DuckGameAudio(false)));
    }
}
