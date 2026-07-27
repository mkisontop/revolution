//! Text-to-speech: WinRT `SpeechSynthesizer` (offline, zero-key) feeding a
//! rodio/WASAPI sink. One thread owns both synthesis and playback so
//! sentence ordering, `TtsFirstAudio` timing, and barge-in are trivially
//! serialized.

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use revolution_core::config::Tts;
use revolution_core::orchestrator::InputEvent;
use windows::core::HSTRING;
use windows::Media::SpeechSynthesis::SpeechSynthesizer;
use windows::Storage::Streams::DataReader;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::msg::{LoopMsg, TtsCmd};
use crate::util::now_ms;

/// Create a synthesizer honoring the configured voice + rate.
/// Returns (synth, description) — description feeds the panel status.
pub fn make_synth(cfg: &Tts) -> windows::core::Result<(SpeechSynthesizer, String)> {
    let synth = SpeechSynthesizer::new()?;
    let mut desc = "default voice".to_string();
    if let Some(want) = cfg.voice.as_ref().map(|v| v.to_lowercase()) {
        let all = SpeechSynthesizer::AllVoices()?;
        for i in 0..all.Size()? {
            let v = all.GetAt(i)?;
            let name = v.DisplayName()?.to_string();
            if name.to_lowercase().contains(&want) {
                synth.SetVoice(&v)?;
                desc = name;
                break;
            }
        }
    } else if let Ok(v) = synth.Voice() {
        if let Ok(n) = v.DisplayName() {
            desc = n.to_string();
        }
    }
    if (cfg.rate - 1.0).abs() > f64::EPSILON {
        synth.Options()?.SetSpeakingRate(cfg.rate.clamp(0.5, 3.0))?;
    }
    Ok((synth, desc))
}

/// Blocking synthesis of one sentence to WAV bytes.
pub fn synth_wav(synth: &SpeechSynthesizer, text: &str) -> windows::core::Result<Vec<u8>> {
    let stream = synth
        .SynthesizeTextToStreamAsync(&HSTRING::from(text))?
        .get()?;
    let size = stream.Size()? as u32;
    let input = stream.GetInputStreamAt(0)?;
    let reader = DataReader::CreateDataReader(&input)?;
    reader.LoadAsync(size)?.get()?;
    let mut bytes = vec![0u8; size as usize];
    reader.ReadBytes(&mut bytes)?;
    Ok(bytes)
}

/// Spawn the TTS thread. Sentences stream in from the LLM task; timing
/// events (`TtsFirstAudio`, `PlaybackFinished`) flow back to the run-loop.
pub fn spawn(cfg: Tts, loop_tx: Sender<LoopMsg>) -> Sender<TtsCmd> {
    let (tx, rx) = std::sync::mpsc::channel::<TtsCmd>();
    std::thread::Builder::new()
        .name("tts".into())
        .spawn(move || run(cfg, loop_tx, rx))
        .expect("spawn tts thread");
    tx
}

fn run(cfg: Tts, loop_tx: Sender<LoopMsg>, rx: Receiver<TtsCmd>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let synth = match make_synth(&cfg) {
        Ok((s, _)) => s,
        Err(e) => {
            eprintln!("tts unavailable: {e}");
            // Drain forever so senders never block; report errors per utterance.
            while let Ok(cmd) = rx.recv() {
                if matches!(cmd, TtsCmd::EndOfUtterance) {
                    let _ = loop_tx.send(LoopMsg::Input(InputEvent::Error {
                        ms: now_ms(),
                        what: "TTS unavailable".into(),
                    }));
                }
            }
            return;
        }
    };
    let Ok((_stream, stream_handle)) = rodio::OutputStream::try_default() else {
        eprintln!("tts: no audio output device");
        while let Ok(cmd) = rx.recv() {
            if matches!(cmd, TtsCmd::EndOfUtterance) {
                let _ = loop_tx.send(LoopMsg::Input(InputEvent::Error {
                    ms: now_ms(),
                    what: "no audio output device".into(),
                }));
            }
        }
        return;
    };

    let mut sink: Option<rodio::Sink> = None;
    let mut first_audio_sent = false;
    let mut pending: VecDeque<TtsCmd> = VecDeque::new();

    loop {
        let cmd = match pending.pop_front() {
            Some(c) => c,
            None => match rx.recv() {
                Ok(c) => c,
                Err(_) => return,
            },
        };
        match cmd {
            TtsCmd::Sentence(text) => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                let wav = match synth_wav(&synth, &text) {
                    Ok(w) => w,
                    Err(e) => {
                        let _ = loop_tx.send(LoopMsg::Input(InputEvent::Error {
                            ms: now_ms(),
                            what: format!("TTS synth failed: {e}"),
                        }));
                        continue;
                    }
                };
                // A barge-in may have arrived while we were synthesizing.
                let mut stopped = false;
                while let Ok(c) = rx.try_recv() {
                    if matches!(c, TtsCmd::Stop) {
                        stopped = true;
                        pending.clear();
                        break;
                    }
                    pending.push_back(c);
                }
                if stopped {
                    if let Some(s) = sink.take() {
                        s.stop();
                    }
                    first_audio_sent = false;
                    continue;
                }
                match rodio::Decoder::new(Cursor::new(wav)) {
                    Ok(source) => {
                        let s = match &sink {
                            Some(s) => s,
                            None => {
                                match rodio::Sink::try_new(&stream_handle) {
                                    Ok(new_sink) => {
                                        sink = Some(new_sink);
                                        sink.as_ref().unwrap()
                                    }
                                    Err(e) => {
                                        let _ = loop_tx.send(LoopMsg::Input(InputEvent::Error {
                                            ms: now_ms(),
                                            what: format!("audio sink: {e}"),
                                        }));
                                        continue;
                                    }
                                }
                            }
                        };
                        s.append(source);
                        if !first_audio_sent {
                            first_audio_sent = true;
                            let _ = loop_tx
                                .send(LoopMsg::Input(InputEvent::TtsFirstAudio { ms: now_ms() }));
                        }
                    }
                    Err(e) => eprintln!("tts wav decode: {e}"),
                }
            }
            TtsCmd::EndOfUtterance => {
                // Wait for the sink to drain, staying responsive to Stop.
                let finished = loop {
                    let empty = sink.as_ref().map(|s| s.empty()).unwrap_or(true);
                    if empty {
                        break true;
                    }
                    match rx.recv_timeout(Duration::from_millis(40)) {
                        Ok(TtsCmd::Stop) => {
                            if let Some(s) = sink.take() {
                                s.stop();
                            }
                            break false;
                        }
                        Ok(other) => pending.push_back(other),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                };
                if finished && first_audio_sent {
                    let _ = loop_tx
                        .send(LoopMsg::Input(InputEvent::PlaybackFinished { ms: now_ms() }));
                }
                first_audio_sent = false;
                sink = None;
            }
            TtsCmd::Stop => {
                if let Some(s) = sink.take() {
                    s.stop();
                }
                first_audio_sent = false;
                pending.clear();
            }
        }
    }
}
