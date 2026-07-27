//! Screen capture service: Windows.Graphics.Capture (WGC) frames →
//! grayscale thumbnail → dHash → `ChangeGate` → JPEG encode → shared
//! `KeyframeRing` (RAM only, per DESIGN.md §12).
//!
//! The pet/panel windows are excluded from capture via
//! `WDA_EXCLUDEFROMCAPTURE` (see `main.rs`), so monitor capture sees only
//! the game underneath.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use revolution_core::vision::dhash::{dhash64, GrayThumb};
use revolution_core::vision::gate::{ChangeGate, GateConfig, GateDecision, PromoteReason};
use revolution_core::vision::ring::{Keyframe, KeyframeRing};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::util::now_ms;

/// Long edge of keyframe JPEGs (DESIGN.md §5.3).
const JPEG_LONG_EDGE: u32 = 1344;
const JPEG_QUALITY: u8 = 80;
/// Ambient sampling interval: hash at most this often.
const SAMPLE_MS: u64 = 250;
/// Ring bounds: 60 frames / 30 MB.
const RING_FRAMES: usize = 60;
const RING_BYTES: usize = 30 << 20;

/// State shared between the capture callback, the run-loop, and the UI.
pub struct CaptureShared {
    pub ring: Mutex<KeyframeRing>,
    gate: Mutex<ChangeGate>,
    /// Set by `Action::CaptureFreshFrame`: promote the next frame unconditionally.
    pub fresh_request: AtomicBool,
    /// Privacy gate (game foreground / capture paused). When false frames are dropped.
    pub enabled: AtomicBool,
    pub frames_seen: AtomicU64,
    pub keyframes: AtomicU64,
    pub last_ms: AtomicU64,
    last_sample_ms: AtomicU64,
    /// Most recent capture error, for the panel status.
    pub error: Mutex<Option<String>>,
    /// Source frame dimensions (status/smoke reporting).
    pub last_dims: Mutex<(u32, u32)>,
}

impl CaptureShared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            ring: Mutex::new(KeyframeRing::new(RING_FRAMES, RING_BYTES)),
            gate: Mutex::new(ChangeGate::new(GateConfig::default())),
            fresh_request: AtomicBool::new(false),
            enabled: AtomicBool::new(true),
            frames_seen: AtomicU64::new(0),
            keyframes: AtomicU64::new(0),
            last_ms: AtomicU64::new(0),
            last_sample_ms: AtomicU64::new(0),
            error: Mutex::new(None),
            last_dims: Mutex::new((0, 0)),
        })
    }

    /// Pause capture and wipe the ring (privacy: game closed / not foreground).
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        if !on {
            self.ring.lock().unwrap().clear();
        }
    }
}

/// Convert an RGBA frame to a luma thumbnail sized for hashing.
fn luma_thumb(rgba: &[u8], w: u32, h: u32) -> GrayThumb {
    // Stride-sample down to roughly 320 wide before the box resize — a full
    // luma pass at 4K is wasted work for a 64-bit hash.
    let step = (w / 320).max(1);
    let tw = w / step;
    let th = (h / step).max(1);
    let mut data = Vec::with_capacity((tw * th) as usize);
    for y in 0..th {
        let sy = y * step;
        for x in 0..tw {
            let sx = x * step;
            let i = ((sy * w + sx) * 4) as usize;
            let (r, g, b) = (rgba[i] as u32, rgba[i + 1] as u32, rgba[i + 2] as u32);
            data.push(((r * 77 + g * 150 + b * 29) >> 8) as u8);
        }
    }
    GrayThumb::new(tw, th, data).box_resize(320, 180.min(th))
}

/// Box-filter an RGBA frame down to `JPEG_LONG_EDGE` and encode as JPEG (RGB).
fn encode_jpeg(rgba: &[u8], w: u32, h: u32) -> anyhow::Result<Vec<u8>> {
    let long = w.max(h);
    let (tw, th) = if long <= JPEG_LONG_EDGE {
        (w, h)
    } else {
        let num = JPEG_LONG_EDGE;
        ((w * num / long).max(1), (h * num / long).max(1))
    };
    let mut rgb = Vec::with_capacity((tw * th * 3) as usize);
    for ty in 0..th {
        let y0 = ty * h / th;
        let y1 = ((ty + 1) * h / th).max(y0 + 1).min(h);
        for tx in 0..tw {
            let x0 = tx * w / tw;
            let x1 = ((tx + 1) * w / tw).max(x0 + 1).min(w);
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = ((y * w + x) * 4) as usize;
                    r += rgba[i] as u32;
                    g += rgba[i + 1] as u32;
                    b += rgba[i + 2] as u32;
                    n += 1;
                }
            }
            rgb.extend_from_slice(&[(r / n) as u8, (g / n) as u8, (b / n) as u8]);
        }
    }
    let mut out = Vec::new();
    let enc = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
    enc.encode(&rgb, tw as u16, th as u16, jpeg_encoder::ColorType::Rgb)?;
    Ok(out)
}

/// WGC callback handler. `Flags` carries the shared state in.
pub struct CaptureHandler {
    shared: Arc<CaptureShared>,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = Arc<CaptureShared>;
    type Error = anyhow::Error;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { shared: ctx.flags })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let s = &self.shared;
        s.frames_seen.fetch_add(1, Ordering::Relaxed);
        if !s.enabled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let now = now_ms();
        let fresh = s.fresh_request.swap(false, Ordering::Relaxed);
        if !fresh && now.saturating_sub(s.last_sample_ms.load(Ordering::Relaxed)) < SAMPLE_MS {
            return Ok(());
        }
        s.last_sample_ms.store(now, Ordering::Relaxed);

        let (w, h) = (frame.width(), frame.height());
        *s.last_dims.lock().unwrap() = (w, h);
        let mut buf = frame.buffer()?;
        let rgba = buf.as_nopadding_buffer()?;

        let hash = dhash64(&luma_thumb(rgba, w, h));
        let decision = if fresh {
            // PTT-down: the freshest possible frame, promoted unconditionally.
            Some(60)
        } else {
            match s.gate.lock().unwrap().decide(now, hash) {
                GateDecision::Promote(PromoteReason::Changed { distance }) => Some(distance),
                GateDecision::Promote(_) => Some(0),
                GateDecision::Drop => None,
            }
        };
        if let Some(change_score) = decision {
            let jpeg = encode_jpeg(rgba, w, h)?;
            s.ring.lock().unwrap().push(Keyframe {
                ts_ms: now,
                hash,
                jpeg,
                ocr_text: None,
                change_score,
            });
            s.keyframes.fetch_add(1, Ordering::Relaxed);
            s.last_ms.store(now, Ordering::Relaxed);
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Start capturing the primary monitor on a free-threaded WGC session.
/// Returns an error string (for status) instead of panicking when WGC is
/// unavailable (RDP session, unsupported GPU, …).
pub fn start(shared: Arc<CaptureShared>) {
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            let monitor = match Monitor::primary() {
                Ok(m) => m,
                Err(e) => {
                    *shared.error.lock().unwrap() = Some(format!("no primary monitor: {e}"));
                    return;
                }
            };
            let settings = Settings::new(
                monitor,
                CursorCaptureSettings::WithoutCursor,
                DrawBorderSettings::WithoutBorder,
                SecondaryWindowSettings::Default,
                // Custom intervals need Win11 24H2+; SAMPLE_MS throttles in
                // software instead, portably.
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                ColorFormat::Rgba8,
                shared.clone(),
            );
            // Blocks for the life of the session (its own message pump).
            if let Err(e) = CaptureHandler::start(settings) {
                *shared.error.lock().unwrap() = Some(format!("capture session ended: {e}"));
            }
        })
        .expect("spawn capture thread");
}
