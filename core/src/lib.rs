//! # revolution-core
//!
//! Platform-independent engine for the Revolution AI gaming companion
//! (see `docs/DESIGN.md` in the repository root).
//!
//! Everything in this crate is **sans-IO**: no network, no OS capture, no audio.
//! It contains the logic that must be correct and fast — provider request
//! building + stream parsing, change-gated keyframe selection, the memory
//! store, the cache-friendly prompt builder, and the push-to-talk
//! orchestrator state machine — all unit-testable on any OS. The Windows
//! shell (Tauri) supplies real capture/audio/hotkey inputs and an HTTP/WS
//! transport, and renders the actions this crate emits.

pub mod config;
pub mod deep;
pub mod gamedetect;
pub mod memory;
pub mod orchestrator;
pub mod prompt;
pub mod providers;
pub mod vision;
