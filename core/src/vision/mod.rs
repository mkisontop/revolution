//! Screen-understanding math: perceptual hashing, the change gate that
//! decides which frames become keyframes, and the in-RAM keyframe ring.

pub mod dhash;
pub mod gate;
pub mod ring;
