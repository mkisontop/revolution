//! The change gate: decides which captured frames get promoted to keyframes.
//!
//! Policy (DESIGN.md §5.2): promote when the perceptual hash moved at least
//! `hamming_threshold` bits AND at least `min_interval_ms` passed since the
//! last keyframe; additionally force a heartbeat keyframe every
//! `heartbeat_ms` so the ring never goes stale on static scenes.

use super::dhash::hamming;

#[derive(Clone, Debug)]
pub struct GateConfig {
    pub hamming_threshold: u32,
    pub min_interval_ms: u64,
    pub heartbeat_ms: u64,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            hamming_threshold: 12,
            min_interval_ms: 1_000,
            heartbeat_ms: 20_000,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PromoteReason {
    FirstFrame,
    Changed { distance: u32 },
    Heartbeat,
}

#[derive(Debug, PartialEq, Eq)]
pub enum GateDecision {
    Promote(PromoteReason),
    Drop,
}

pub struct ChangeGate {
    cfg: GateConfig,
    /// (timestamp_ms, hash) of the last promoted keyframe.
    last: Option<(u64, u64)>,
}

impl ChangeGate {
    pub fn new(cfg: GateConfig) -> Self {
        Self { cfg, last: None }
    }

    pub fn decide(&mut self, now_ms: u64, hash: u64) -> GateDecision {
        match self.last {
            None => {
                self.last = Some((now_ms, hash));
                GateDecision::Promote(PromoteReason::FirstFrame)
            }
            Some((ts, prev_hash)) => {
                let elapsed = now_ms.saturating_sub(ts);
                if elapsed >= self.cfg.heartbeat_ms {
                    self.last = Some((now_ms, hash));
                    return GateDecision::Promote(PromoteReason::Heartbeat);
                }
                let distance = hamming(prev_hash, hash);
                if distance >= self.cfg.hamming_threshold && elapsed >= self.cfg.min_interval_ms {
                    self.last = Some((now_ms, hash));
                    GateDecision::Promote(PromoteReason::Changed { distance })
                } else {
                    GateDecision::Drop
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANGED: u64 = 0xFFFF_0000_FFFF_0000; // 32 bits away from 0

    #[test]
    fn first_frame_always_promotes() {
        let mut g = ChangeGate::new(GateConfig::default());
        assert_eq!(g.decide(0, 0), GateDecision::Promote(PromoteReason::FirstFrame));
    }

    #[test]
    fn static_scene_drops_until_heartbeat() {
        let mut g = ChangeGate::new(GateConfig::default());
        g.decide(0, 0);
        for t in (1_000..19_000).step_by(1_000) {
            assert_eq!(g.decide(t, 0), GateDecision::Drop, "at t={t}");
        }
        assert_eq!(g.decide(20_000, 0), GateDecision::Promote(PromoteReason::Heartbeat));
    }

    #[test]
    fn big_change_promotes_after_min_interval() {
        let mut g = ChangeGate::new(GateConfig::default());
        g.decide(0, 0);
        // Big change but too soon → suppressed by min interval.
        assert_eq!(g.decide(500, CHANGED), GateDecision::Drop);
        // Same change after the interval → promoted with its distance.
        assert_eq!(
            g.decide(1_500, CHANGED),
            GateDecision::Promote(PromoteReason::Changed { distance: 32 })
        );
    }

    #[test]
    fn small_change_never_promotes_before_heartbeat() {
        let mut g = ChangeGate::new(GateConfig::default());
        g.decide(0, 0);
        // 4-bit wiggle (HUD counters, minimap dots).
        assert_eq!(g.decide(5_000, 0b1111), GateDecision::Drop);
    }
}
