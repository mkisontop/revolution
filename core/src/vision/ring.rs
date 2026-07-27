//! In-RAM keyframe ring buffer + question-time frame selection.
//!
//! Privacy property (DESIGN.md §12): this buffer is the *only* place frames
//! live. It is bounded by count and bytes, never touches disk, and frames
//! leave the process only when selected for an explicit request.

use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct Keyframe {
    pub ts_ms: u64,
    pub hash: u64,
    /// Encoded JPEG bytes (long edge ≤ ~1344px per DESIGN.md §5.3).
    pub jpeg: Vec<u8>,
    /// Optional OCR text extracted from this frame (trigger channel).
    pub ocr_text: Option<String>,
    /// Hamming distance that promoted this frame (0 for first/heartbeat).
    pub change_score: u32,
}

pub struct KeyframeRing {
    max_frames: usize,
    max_bytes: usize,
    bytes: usize,
    frames: VecDeque<Keyframe>,
}

impl KeyframeRing {
    pub fn new(max_frames: usize, max_bytes: usize) -> Self {
        Self {
            max_frames,
            max_bytes,
            bytes: 0,
            frames: VecDeque::new(),
        }
    }

    pub fn push(&mut self, f: Keyframe) {
        self.bytes += f.jpeg.len();
        self.frames.push_back(f);
        while self.frames.len() > self.max_frames || self.bytes > self.max_bytes {
            match self.frames.pop_front() {
                Some(old) => self.bytes -= old.jpeg.len(),
                None => break,
            }
        }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn byte_size(&self) -> usize {
        self.bytes
    }

    pub fn latest(&self) -> Option<&Keyframe> {
        self.frames.back()
    }

    /// Wipe everything (game closed / capture paused / app exit).
    pub fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
    }

    /// Select frames to attach to a question: always the freshest frame,
    /// plus up to `k-1` earlier keyframes with the highest change scores,
    /// spaced at least `min_spacing_ms` apart. Returned in chronological
    /// order (oldest first) for the prompt.
    pub fn select_for_question(&self, k: usize, min_spacing_ms: u64) -> Vec<&Keyframe> {
        let mut out: Vec<&Keyframe> = Vec::new();
        let Some(latest) = self.frames.back() else {
            return out;
        };
        out.push(latest);
        if k > 1 {
            let mut cands: Vec<&Keyframe> = self.frames.iter().rev().skip(1).collect();
            cands.sort_by(|a, b| b.change_score.cmp(&a.change_score));
            for c in cands {
                if out.len() >= k {
                    break;
                }
                if out.iter().all(|s| s.ts_ms.abs_diff(c.ts_ms) >= min_spacing_ms) {
                    out.push(c);
                }
            }
        }
        out.sort_by_key(|f| f.ts_ms);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kf(ts_ms: u64, jpeg_len: usize, change_score: u32) -> Keyframe {
        Keyframe {
            ts_ms,
            hash: ts_ms,
            jpeg: vec![0u8; jpeg_len],
            ocr_text: None,
            change_score,
        }
    }

    #[test]
    fn evicts_by_frame_count_and_bytes() {
        let mut r = KeyframeRing::new(3, 1_000);
        r.push(kf(1, 400, 0));
        r.push(kf(2, 400, 0));
        r.push(kf(3, 400, 0)); // 1200 bytes > 1000 → oldest evicted
        assert_eq!(r.len(), 2);
        assert!(r.byte_size() <= 1_000);
        r.push(kf(4, 10, 0));
        r.push(kf(5, 10, 0)); // count cap 3
        assert_eq!(r.len(), 3);
        assert_eq!(r.latest().unwrap().ts_ms, 5);
    }

    #[test]
    fn selection_prefers_fresh_plus_high_change_with_spacing() {
        let mut r = KeyframeRing::new(16, 1 << 20);
        r.push(kf(0, 10, 0)); // first frame
        r.push(kf(6_000, 10, 40)); // big event
        r.push(kf(7_000, 10, 14)); // small change right after (should lose to spacing)
        r.push(kf(20_000, 10, 22)); // medium event
        r.push(kf(30_000, 10, 5)); // freshest
        let sel = r.select_for_question(3, 5_000);
        let times: Vec<u64> = sel.iter().map(|f| f.ts_ms).collect();
        assert_eq!(times, vec![6_000, 20_000, 30_000]);
    }

    #[test]
    fn selection_from_single_frame() {
        let mut r = KeyframeRing::new(4, 1 << 20);
        r.push(kf(42, 10, 0));
        let sel = r.select_for_question(3, 5_000);
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].ts_ms, 42);
    }

    #[test]
    fn clear_wipes_bytes() {
        let mut r = KeyframeRing::new(4, 1 << 20);
        r.push(kf(1, 100, 0));
        r.clear();
        assert!(r.is_empty());
        assert_eq!(r.byte_size(), 0);
    }
}
