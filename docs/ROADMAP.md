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

## Phase 1 — Windows bring-up: one real conversation (code complete + machine-validated 2026-07-28)

- [x] WGC capture via `windows-capture` → change gate → ring buffer (real frames) — **validated on the target machine**: 2560×1440 → first keyframe in ~400 ms (`--smoke`)
- [x] LL keyboard hook PTT (Page Up, configurable, hold or toggle) — `desktop/src-tauri/src/hotkey.rs`
- [x] WASAPI mic (always-open, retain-while-held) + playback + game-session ducking — mic/output/session enumeration validated by `--smoke`
- [x] Hot lane wired end-to-end: PTT → batch Whisper (any OpenAI-compatible endpoint; Groq free tier documented) → configured `qa` model → **WinRT Windows TTS v0** (zero-key; 6 ms synth validated) → speakers, with sentence-streamed synthesis and barge-in. Deepgram/Cartesia/ElevenLabs remain the streaming upgrades.
- [x] Tauri v2 shell: transparent always-on-top pet (state animations, speech bubble, capture-indicator, drag) + panel (transcript, typed asks, latency ledger, key vault, config editor); pet/panel excluded from their own capture via `WDA_EXCLUDEFROMCAPTURE`
- [x] **Measure the §4 latency table for real** — measured live 2026-07-28 (vision turn TTFT 1.9–4.9 s depending on brain/effort; ledger in the panel per turn)

**Exit: ask a question about a running game and hear a correct, screen-grounded spoken answer; PresentMon FPS impact check remains open.** ✅ *(validated live on Palworld)*

## Phase 2 — The friend: memory, identity, polish (shipped 2026-07-28)

- [x] Game detection live (foreground poll + Steam manifests + Discord DB); per-game profile cards flowing into every prompt
- [x] `remember`/`update_profile` tools live (OpenAI-compat tool wire); session-end summarizer (async on game switch, blocking-with-deadline on quit); hybrid retrieval injection
- [x] Model-written compaction (heuristic fallback offline); cost meter + monthly budget cap (enforced when token prices are set); settings UI (hotkey, voice, models, privacy, companion) writing config via toml_edit
- [x] Pet states/animations (blink/gaze/breathe/sleep/think/speak/celebrate), chat panel with transcript + typed input, first-run onboarding wizard, system tray (mute/autostart/quit)
- [x] Hype mode (opt-in ambient lane) with cadence floor, new-frame gating, PASS-token silence, barge-in safety
- [ ] Windows OCR trigger channel (with package identity)

**Exit: two gaming sessions a week apart — the pet correctly recalls build/goals/events from session 1 in session 2, unprompted.** *(machinery live; awaiting the second week's session)*

## Phase 3 — The expert: browsing + deep lane

- 3a: native web-search tools enabled in `qa` (two-beat answers) and a background `deep` job runner with tip cards
- 3b: harness sidecar if jobs outgrow single-request search — Claude Agent SDK (default) or Hermes Agent via `hermes acp` (alternative; user preference)
- Per-game adapters: LoL Live Client API, CS2/Dota GSI; IGDB + wiki-API integration for the researcher
- Optional Pipeline B: `gpt-realtime-2.1` speech-to-speech mode

**Exit: "what's the best Pal for this task?" gets a fresh-wiki-grounded answer; "research my build" delivers a tip card minutes later without interrupting play.**

## Phase 4 — Ship

- [x] NSIS installer (per-user, `tauri build`) + first-run onboarding (brain preset → key → live test) — 2026-07-28
- MSIX + sparse identity (for the OCR channel), privacy walkthrough polish
- Compatibility matrix testing (incl. FACEIT/Vanguard titles), auto-hide fallbacks
- Persona/voice packs, pet art, streamer mode
- Docs, telemetry-free crash reporting choice, release

**Exit: a stranger installs it, pastes one API key, and has the "friend who watches" experience in under 5 minutes.**
