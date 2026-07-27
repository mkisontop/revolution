//! Embedding abstraction.
//!
//! Production (Windows shell) plugs in `fastembed` (bge-small-en-v1.5 int8,
//! 384-d, CPU). For tests and keyless dev this crate ships a deterministic
//! character-trigram hashing embedder — crude, but similar texts land near
//! each other, which is all the store's ranking logic needs.

pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    /// Returns an L2-normalized vector of `dim()` floats.
    fn embed(&self, text: &str) -> Vec<f32>;
}

pub struct HashEmbedder {
    pub dim: usize,
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self { dim: 256 }
    }
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        let normalized: Vec<char> = text
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect();
        if normalized.len() >= 3 {
            for w in normalized.windows(3) {
                if w.iter().all(|c| *c == ' ') {
                    continue;
                }
                let mut h: u64 = 0xcbf29ce484222325;
                for c in w {
                    h ^= *c as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
                v[(h % self.dim as u64) as usize] += 1.0;
            }
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_texts_are_closer_than_unrelated() {
        let e = HashEmbedder::default();
        let a = e.embed("player is running a dexterity katana build");
        let b = e.embed("the dexterity katana build the player runs");
        let c = e.embed("bought 40 iron ore at the mountain shop");
        assert!(cosine(&a, &b) > cosine(&a, &c));
        assert!(cosine(&a, &b) > 0.5);
    }

    #[test]
    fn deterministic_and_normalized() {
        let e = HashEmbedder::default();
        let a = e.embed("hello world");
        let b = e.embed("hello world");
        assert_eq!(a, b);
        let n: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4);
    }
}
