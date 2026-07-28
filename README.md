# Revolution

**A real-time AI gaming companion for Windows 11** — a desktop pet that sits on your screen, sees your game the moment you ask, and talks to you like an expert friend on the couch next to you.

Hold **Page Up**, ask *"what's the best Pal for this task?"*, release — and within ~1–2 seconds a voice answers, having just looked at your actual screen, with your build and goals in memory, and with live web search behind it when current patch knowledge matters. **Bring your own AI:** plug in your key for Grok, Gemini, a ChatGPT model, or Claude — per role, your choice, no credit meter.

## What it is

- 🐾 **A pet on your desktop** — a small always-on-top companion (transparent, click-through) with an expandable chat panel and a visible capture indicator.
- 👀 **Real eyes on your screen** — continuous low-cost capture (Windows Graphics Capture, the OBS API) → change-gated keyframes in a RAM-only ring buffer; frames leave your PC only when you ask.
- 🎙️ **Push-to-talk voice** — hold a key, talk, release; streaming STT finishes as you let go; the answer streams back as voice while game audio ducks. Optional speech-to-speech mode (gpt-realtime).
- 🧠 **Your model, your key** — provider-agnostic adapters (OpenAI-compatible incl. xAI Grok/OpenRouter/Groq, Google Gemini, Anthropic Claude) with per-role routing: fast model for live chat, cheap model for ambient banter, strong model for research.
- 🔎 **Fresh knowledge** — native web-search tools in the live loop ("checking the wiki real quick…") plus an async deep-research lane that delivers tip cards without interrupting play.
- 📚 **Memory that makes it a friend** — per-game profile card, session episodes, deduplicated facts (SQLite, local-only, viewable/deletable). It remembers your build, your goals, your running jokes.
- ⚡ **Performance-first** — no local GPU inference while you game, near-zero idle cost, and a published anti-cheat-safe envelope: no injection, no memory reading, no input automation.

## Status

**Phases 0–2 shipped and machine-validated (2026-07-28)** — the engine, the Windows shell, and the companion layer are all live: WGC capture with ask-time frame guarantees, PgUp push-to-talk with STT defection guards, streaming voice answers with barge-in, web tool-search when the brain is unsure, per-game memory with the brain's own `remember`/`update_profile` tools, session episode summaries, an opt-in ambient hype lane, a living animated pet, a hub panel with first-run onboarding + full settings UI, a system tray, budget metering, and an NSIS installer. **72/72 tests, zero clippy warnings, live smoke-verified.**

```sh
cargo test --workspace                  # 72 passed — no keys, no network needed
cargo run -p revolution-desktop         # run it (dev)
target\debug\revolution-desktop --smoke # headless hardware validation
cd desktop/src-tauri && tauri build     # release + NSIS installer
```

| Document | What's in it |
|---|---|
| [`docs/DESIGN.md`](docs/DESIGN.md) | Full system design: architecture, latency budget, capture pipeline, BYO provider layer, prompt/caching strategy, voice stack, memory, deep lane, anti-cheat policy, privacy, cost model |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Phased build plan with exit criteria (Phase 0 ✅ → Windows bring-up → memory → browsing/deep lane → ship) |
| [`docs/RESEARCH.md`](docs/RESEARCH.md) | Research appendix: verified findings, benchmarks, competitive deep-dive (Questie.ai et al.), sources |
| [`docs/VALIDATION.md`](docs/VALIDATION.md) | What's tested and how; what still needs a Win11 machine and live keys |
| [`desktop/README.md`](desktop/README.md) | Windows shell bring-up guide: crate wiring map from OS glue to the tested core APIs |

## Repo layout

```
core/                  # revolution-core: the tested sans-IO engine
  src/providers/       #   OpenAI-compat (Grok/…), Gemini, Anthropic adapters + SSE
  src/vision/          #   dHash, change gate, keyframe ring
  src/memory/          #   SQLite store, embeddings, fact gate, hybrid retrieval
  src/prompt.rs        #   cache-stable prompt builder + compaction + personas
  src/orchestrator.rs  #   PTT state machine, barge-in, latency budget ledger
  src/gamedetect.rs    #   Steam manifest / detectable-DB parsers
  src/config.rs        #   BYOK per-role config (example in-file)
  src/deep.rs          #   research-job request builder (web search forced on)
  tests/turn_flow.rs   #   end-to-end simulated companion turn
desktop/               # Windows Tauri shell (Phase 1) — bring-up guide
docs/                  # design, roadmap, research, validation
```

## The 30-second architecture

```
Pet + chat UI ......... Tauri v2 (Rust core, WebView2 UI) — ~30–50 MB RAM, <1% CPU idle
Screen eyes ........... WGC → dHash change gate → keyframe ring (RAM-only, upload-on-ask)
Ears / voice .......... LL-hook PgUp PTT → streaming STT (Deepgram) → streaming TTS (Cartesia/ElevenLabs)
Brain ................. YOUR endpoint+key+model per role (Grok / Gemini / GPT / Claude) + native web search
Memory ................ SQLite + FTS5 + local embeddings; per-game profile card + episodes + gated facts
Deep research ......... background jobs → tip cards (native search tools; Agent SDK / Hermes sidecar later)
Anti-cheat stance ..... watch & talk only: no injection, no memory reads, no input automation
```

Voice-to-voice target: **< 2s** from key release to first spoken word (measured budget ~0.8–1.8s; the orchestrator ships a per-stage latency ledger so real numbers replace estimates on your machine).
