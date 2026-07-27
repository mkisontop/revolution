//! Microphone capture (cpal/WASAPI). The input stream is opened once at
//! startup and runs continuously; samples are only *retained* while the
//! recording flag is up (PTT held), so key-down → capture start is
//! effectively instant.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub struct MicHandle {
    recording: Arc<AtomicBool>,
    buf: Arc<Mutex<Vec<f32>>>,
    sample_rate: Arc<AtomicU32>,
    pub status: Arc<Mutex<String>>,
}

impl MicHandle {
    /// Begin retaining samples (PTT down).
    pub fn start(&self) {
        self.buf.lock().unwrap().clear();
        self.recording.store(true, Ordering::Relaxed);
    }

    /// Stop retaining and return the take as a 16-bit mono WAV, or None if
    /// nothing usable was captured.
    pub fn stop_take_wav(&self) -> Option<Vec<u8>> {
        self.recording.store(false, Ordering::Relaxed);
        let samples: Vec<f32> = std::mem::take(&mut *self.buf.lock().unwrap());
        let rate = self.sample_rate.load(Ordering::Relaxed);
        if rate == 0 || samples.len() < (rate / 4) as usize {
            return None; // under ~250ms of audio — nothing intelligible
        }
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec).ok()?;
            for s in samples {
                writer
                    .write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                    .ok()?;
            }
            writer.finalize().ok()?;
        }
        Some(cursor.into_inner())
    }
}

/// Open the default input device on a dedicated thread (cpal streams are not
/// `Send`) and return the control handle. Failure is non-fatal: status says
/// why and typed questions still work.
pub fn spawn() -> MicHandle {
    let recording = Arc::new(AtomicBool::new(false));
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sample_rate = Arc::new(AtomicU32::new(0));
    let status = Arc::new(Mutex::new("mic: starting…".to_string()));

    let handle = MicHandle {
        recording: recording.clone(),
        buf: buf.clone(),
        sample_rate: sample_rate.clone(),
        status: status.clone(),
    };

    std::thread::Builder::new()
        .name("mic".into())
        .spawn(move || {
            let host = cpal::default_host();
            let Some(device) = host.default_input_device() else {
                *status.lock().unwrap() = "mic: no input device".into();
                return;
            };
            let name = device.name().unwrap_or_else(|_| "?".into());
            let config = match device.default_input_config() {
                Ok(c) => c,
                Err(e) => {
                    *status.lock().unwrap() = format!("mic: {e}");
                    return;
                }
            };
            sample_rate.store(config.sample_rate().0, Ordering::Relaxed);
            let channels = config.channels() as usize;

            macro_rules! build {
                ($t:ty, $to_f32:expr) => {{
                    let recording = recording.clone();
                    let buf = buf.clone();
                    device.build_input_stream(
                        &config.clone().into(),
                        move |data: &[$t], _| {
                            if !recording.load(Ordering::Relaxed) {
                                return;
                            }
                            let mut b = buf.lock().unwrap();
                            for frame in data.chunks(channels) {
                                let sum: f32 = frame.iter().map(|s| $to_f32(*s)).sum();
                                b.push(sum / channels as f32);
                            }
                        },
                        |e| eprintln!("mic stream error: {e}"),
                        None,
                    )
                }};
            }

            let stream = match config.sample_format() {
                cpal::SampleFormat::F32 => build!(f32, |s: f32| s),
                cpal::SampleFormat::I16 => build!(i16, |s: i16| s as f32 / i16::MAX as f32),
                cpal::SampleFormat::U16 => {
                    build!(u16, |s: u16| (s as f32 - 32768.0) / 32768.0)
                }
                other => {
                    *status.lock().unwrap() = format!("mic: unsupported format {other:?}");
                    return;
                }
            };
            match stream {
                Ok(s) => {
                    if let Err(e) = s.play() {
                        *status.lock().unwrap() = format!("mic: {e}");
                        return;
                    }
                    *status.lock().unwrap() = format!("mic: {name}");
                    // Keep the stream alive forever.
                    std::thread::park();
                    drop(s);
                }
                Err(e) => *status.lock().unwrap() = format!("mic: {e}"),
            }
        })
        .expect("spawn mic thread");

    handle
}
