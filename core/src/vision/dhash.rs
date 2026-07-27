//! 64-bit difference hash (dHash) over grayscale thumbnails.
//!
//! The capture layer downscales each WGC frame to a small luma thumbnail
//! (e.g. 320×180) on the GPU; this module reduces it to a 64-bit perceptual
//! hash. Consecutive-frame Hamming distance drives the change gate.

/// Row-major 8-bit luma image.
#[derive(Clone, Debug)]
pub struct GrayThumb {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

impl GrayThumb {
    pub fn new(w: u32, h: u32, data: Vec<u8>) -> Self {
        assert_eq!(data.len(), (w * h) as usize, "luma buffer size mismatch");
        Self { w, h, data }
    }

    pub fn from_fn(w: u32, h: u32, f: impl Fn(u32, u32) -> u8) -> Self {
        let mut data = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                data.push(f(x, y));
            }
        }
        Self { w, h, data }
    }

    /// Box-filter (average-pooling) resize. Quality is irrelevant for
    /// hashing; determinism and speed are what matter.
    pub fn box_resize(&self, tw: u32, th: u32) -> GrayThumb {
        assert!(tw > 0 && th > 0);
        let mut out = Vec::with_capacity((tw * th) as usize);
        for ty in 0..th {
            let y0 = (ty * self.h) / th;
            let mut y1 = ((ty + 1) * self.h) / th;
            if y1 <= y0 {
                y1 = (y0 + 1).min(self.h.max(1));
            }
            for tx in 0..tw {
                let x0 = (tx * self.w) / tw;
                let mut x1 = ((tx + 1) * self.w) / tw;
                if x1 <= x0 {
                    x1 = (x0 + 1).min(self.w.max(1));
                }
                let mut sum: u64 = 0;
                let mut count: u64 = 0;
                for y in y0..y1.min(self.h) {
                    for x in x0..x1.min(self.w) {
                        sum += self.data[(y * self.w + x) as usize] as u64;
                        count += 1;
                    }
                }
                out.push(if count == 0 { 0 } else { (sum / count) as u8 });
            }
        }
        GrayThumb { w: tw, h: th, data: out }
    }
}

/// Classic dHash: downsample to 9×8, set one bit per horizontally adjacent
/// pixel pair (left brighter than right).
pub fn dhash64(img: &GrayThumb) -> u64 {
    let small = img.box_resize(9, 8);
    let mut bits: u64 = 0;
    let mut i = 0u32;
    for y in 0..8u32 {
        for x in 0..8u32 {
            let left = small.data[(y * 9 + x) as usize];
            let right = small.data[(y * 9 + x + 1) as usize];
            if left > right {
                bits |= 1u64 << i;
            }
            i += 1;
        }
    }
    bits
}

/// Hamming distance between two hashes (0..=64).
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient_lr() -> GrayThumb {
        GrayThumb::from_fn(320, 180, |x, _| ((x * 255) / 319) as u8)
    }

    #[test]
    fn identical_frames_have_zero_distance() {
        let a = gradient_lr();
        let b = gradient_lr();
        assert_eq!(hamming(dhash64(&a), dhash64(&b)), 0);
    }

    #[test]
    fn very_different_scenes_have_large_distance() {
        // Left-to-right gradient vs. its mirror flips every comparison.
        let a = gradient_lr();
        let b = GrayThumb::from_fn(320, 180, |x, _| (((319 - x) * 255) / 319) as u8);
        let d = hamming(dhash64(&a), dhash64(&b));
        assert!(d > 30, "expected large distance, got {d}");
    }

    #[test]
    fn small_noise_stays_under_change_threshold() {
        let a = gradient_lr();
        // Same scene with tiny deterministic sensor-style noise.
        let b = GrayThumb::from_fn(320, 180, |x, y| {
            let base = ((x * 255) / 319) as i32;
            let noise = ((x * 7 + y * 13) % 5) as i32 - 2;
            (base + noise).clamp(0, 255) as u8
        });
        let d = hamming(dhash64(&a), dhash64(&b));
        assert!(d < 10, "noise should not read as a scene change, got {d}");
    }

    #[test]
    fn resize_handles_tiny_sources() {
        let tiny = GrayThumb::from_fn(4, 3, |x, y| (x * 10 + y) as u8);
        let _ = dhash64(&tiny); // must not panic
    }
}
