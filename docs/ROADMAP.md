# Revolution — Build Roadmap

Each phase has explicit exit criteria. Phases 1+ require a Windows 11 machine; Phase 0 runs anywhere (it was executed in the cloud container that authored this repo — see [VALIDATION.md](VALIDATION.md)).

## Phase 0 — Core engine, tested off-Windows ✅ (this repo, now)

The platform-independent engine as a Rust workspace crate (`core/`), fully unit-tested on Linux:

- [x] Provider adapter layer (OpenAI-compatible incl. xAI Grok / Gemini / Anthropic): request builders + SSE stream parsers, normalized stream events, native web-search tool wiring — golden-tested against each wire format
- [x] Vision math: dHash + Hamming change gate, heartbeat keyframes, keyframe ring buffer with byte budget, question-time frame selection
- [x] Memory store: SQLite schema (games/profiles/episodes/facts), FTS5 keyword + embedding hybrid retrieval, ADD/UPDATE/NOOP fact gate, `Embedder` trait (deterministic test embedder now; fastembed on Windows later)
- [x] Prompt builder: cache-friendly layout (frozen prefix → profile block → rolling turns → hottest question+frames), image budget enforcement, compaction trigger logic
- [x] PTT orchestrator state machine (idle/listening/finalizing/answering/speaking, barge-in) + latency instrumentation against the §4 budget
- [x] Game detection ladder logic: Steam `appmanifest_*.acf` parser + detectable-DB exe matching (pure functions; Win32 process source stubbed)
- [x] Config: per-role provider/model routing, BYO base_url + API key, secret redaction

**Exit criteria: `cargo test` green; adapters' request JSON verified against provider docs; latency-budget arithmetic encoded in tests.** ✅

## Phase 1 — Windows bring-up: one real conversation (~1–2 weeks on a Win11 box)

- WGC capture via `windows-capture` → change gate → ring buffer (real frames)
- LL keyboard hook PTT (Page Up), WASAPI mic capture + playback + game-session ducking
- Wire hot lane end-to-end with the user's real keys: PTT → Deepgram (or Groq Whisper batch v0) → configured `qa` model → Cartesia/ElevenLabs → speakers
- Tauri v2 shell: pet window (transparent/always-on-top/click-through poll) + minimal speech bubble
- **Measure the §4 latency table for real** on the user's GPU/network; record TTFT per candidate model (Grok / Gemini Flash / GPT / Claude) in the debug overlay

**Exit: ask a question about a running game and hear a correct, screen-grounded spoken answer in < 2s p50; PresentMon shows no measurable FPS impact.**

## Phase 2 — The friend: memory, identity, polish

- Game detection live (foreground hook + Discord DB + Steam manifests); per-game profile cards flowing into every prompt
- `remember`/`update_profile` tools live; session-end summarizer; hybrid retrieval injection
- Client-side compaction at thresholds; cost meter + monthly budget cap; settings UI (hotkey, voice, models, privacy)
- Pet states/animations, chat panel with transcript + typed input
- Hype mode (opt-in ambient lane) with rate limiting
- Windows OCR trigger channel (with package identity)

**Exit: two gaming sessions a week apart — the pet correctly recalls build/goals/events from session 1 in session 2, unprompted.**

## Phase 3 — The expert: browsing + deep lane

- 3a: native web-search tools enabled in `qa` (two-beat answers) and a background `deep` job runner with tip cards
- 3b: harness sidecar if jobs outgrow single-request search — Claude Agent SDK (default) or Hermes Agent via `hermes acp` (alternative; user preference)
- Per-game adapters: LoL Live Client API, CS2/Dota GSI; IGDB + wiki-API integration for the researcher
- Optional Pipeline B: `gpt-realtime-2.1` speech-to-speech mode

**Exit: "what's the best Pal for this task?" gets a fresh-wiki-grounded answer; "research my build" delivers a tip card minutes later without interrupting play.**

## Phase 4 — Ship

- Installer (MSIX / NSIS + sparse identity), first-run onboarding (borderless tip, key setup, privacy walkthrough)
- Compatibility matrix testing (incl. FACEIT/Vanguard titles), auto-hide fallbacks
- Persona/voice packs, pet art, streamer mode
- Docs, telemetry-free crash reporting choice, release

**Exit: a stranger installs it, pastes one API key, and has the "friend who watches" experience in under 5 minutes.**
