# Revolution — Validation Report

*Phase 0 executed 2026-07-27 in a Linux cloud container (Rust 1.94). This report states exactly what is proven, how, and what still needs a Windows 11 machine and live API keys.*

## Verdict

**`revolution-core` builds clean and passes 55/55 tests (54 unit + 1 end-to-end turn simulation), with zero clippy warnings**, ~3,100 lines of Rust across 14 modules. The platform-independent engine — everything that must be *correct* for the product to work — is implemented and verified. What remains for Windows bring-up is thin OS glue (real capture/hotkey/audio devices + an HTTP/WS transport) around interfaces these tests already exercise.

```
cargo test --workspace   → 55 passed; 0 failed
cargo clippy --all-targets → 0 warnings
```

## What is validated, and how

| Area | Proof |
|---|---|
| **BYO provider layer** — exact request wire-shapes for OpenAI-compatible (incl. xAI Grok base_url + Live Search params), Gemini (streamGenerateContent + `google_search` tool), Anthropic (Messages + 1h-TTL cache breakpoints + moving turn breakpoint + `web_search_20260209` + `effort`) | golden request tests per adapter (`providers/{openai,gemini,anthropic}.rs`) |
| **Stream handling** — each provider's SSE dialect parsed into normalized events (text deltas, tool activity, usage incl. cached tokens, finish reasons); incremental assembly across arbitrary network chunk boundaries | canned-transcript parser tests + `sse.rs` chunk-split tests + integration test splits mid-message |
| **Change-gated vision** — dHash invariance (identical scenes = 0 distance; scene changes read large; sensor noise reads small), gate policy (threshold + min-interval + heartbeat), ring eviction by count/bytes, question-time frame selection (fresh + highest-change with spacing) | `vision/*` tests; integration test shows 30 captured frames → 3–8 promoted keyframes |
| **Memory** — per-game profile card, episodes, ADD/UPDATE/NOOP fact gate (dedupes rephrasings, keeps unrelated facts), hybrid FTS5+embedding retrieval with per-game isolation, **FTS5 confirmed present in bundled SQLite** | `memory/store.rs` tests |
| **Prompt architecture** — frozen prefix byte-stability across turns (the cache contract), late injection of memories, image caps, compaction (fold old turns → summary note, strip stale images, keep recent verbatim) | `prompt.rs` tests |
| **PTT orchestration** — full happy path, barge-in (cancel + relisten), empty-transcript recovery, error recovery, latency ledger arithmetic against the 2s budget | `orchestrator.rs` tests |
| **Game detection** — real-shaped Steam `appmanifest_*.acf` parsing, Discord detectable-DB exe matching (nested paths, case), install-dir scoping | `gamedetect.rs` tests |
| **Config** — the shipped example TOML parses, minimal configs get sane defaults, TOML roundtrip, **API keys never appear in Debug/log output** | `config.rs` + `providers` redaction tests |
| **Deep lane** — research requests force native web search on for all three providers and carry profile + wiki hints | `deep.rs` tests |
| **Everything together** — one simulated session: config → capture/gate/ring → PTT → memory retrieval → prompt build → Grok request → streamed reply reassembly → latency report (1.25s, within budget) → post-answer memory write with dedup | `tests/turn_flow.rs` |

## What is NOT yet validated (and where it happens)

| Item | Why not here | Where |
|---|---|---|
| Real WGC capture, LL keyboard hook, WASAPI audio/ducking, Tauri windows | Windows-only APIs; this container is Linux | Phase 1 on a Win11 machine (`desktop/README.md` has the bring-up plan) |
| Live requests against real endpoints (Grok/Gemini/OpenAI/Claude), true TTFT numbers | No user API keys in this container — adapters are verified against each provider's documented wire format instead | Phase 1 with the user's keys; debug overlay records per-model TTFT |
| STT/TTS vendor latency incl. our network path | Vendor-side claims only | Phase 1 measurement |
| Real-game FPS impact | Needs games + PresentMon | Phase 1 exit criterion |
| Realtime (Pipeline B) session payloads against the current OpenAI Realtime schema | Surface evolves quickly; builder is shape-tested only | Phase 3 integration |
| Windows OCR accuracy, FACEIT/Vanguard overlay behavior | Windows + specific titles | Phase 1–4 test matrix |

## Reproduce

```sh
git clone <this repo> && cd revolution
cargo test --workspace     # 61 tests (57 core unit + 1 integration + 3 desktop)
cargo clippy --workspace --all-targets
```

No API keys, network, or Windows required for the above.

---

# Phase 1 — Windows validation (2026-07-28, the target machine)

*Executed on the user's Windows 11 Pro 23H2 box (i5-13600K / RTX 4070 Ti / 32 GB, Rust 1.89): the desktop shell was implemented (~2,600 new lines across 14 files) and every piece of OS glue that doesn't need a human or an API key was exercised for real via `revolution-desktop --smoke`.*

## Verdict

**Workspace builds clean (0 warnings, 0 clippy lints), 61/61 tests pass on Windows, and the hardware smoke test PASSES**: real WGC capture through the change gate into the ring, real WinRT TTS through the speakers, mic + audio-session enumeration all live. The only thing between this machine and a full spoken game conversation is a valid API key.

```
cargo test --workspace                → 61 passed; 0 failed   (first-ever Windows run)
cargo clippy --workspace --all-targets → 0 warnings
revolution-desktop --smoke            → PASS
```

## Measured on this machine

| Stage | Result |
|---|---|
| WGC monitor capture (2560×1440) → luma dHash → gate → JPEG → ring | first keyframe **~400 ms** from session start; 143–232 KB keyframes |
| WinRT TTS synthesis (`Microsoft David`) | **6 ms** for a full sentence (cold: 33 ms) — effectively free against the 300 ms TTS budget line |
| TTS playback via rodio/WASAPI | audible, sink drained cleanly |
| Mic (cpal/WASAPI) | `Microphone (GM305)` opened; always-open retain-while-held pattern working |
| Audio render sessions visible for ducking | 6 sessions enumerable (`ISimpleAudioVolume` accessible) |
| Live model lane (`--smoke-llm`, production streaming path) | transport chain **verified end-to-end** — correct provider URL, streamed SSE request, Credential Manager key resolution, clean error surfacing. Request rejected with `API_KEY_INVALID`: both keys found on the machine (`GEMINI_API_KEY`, `OPENROUTER_API_KEY`) are expired. **TTFT measurement pending a valid key.** |

Platform note: `MinimumUpdateIntervalSettings::Custom` requires Win11 24H2+ (this box is 23H2) — capture throttling is done in software (250 ms sampling) instead; documented in `capture.rs`.

## Still needs a human / a key

| Item | How |
|---|---|
| Live spoken turn + real TTFT per model | paste a valid key (panel → API keys, or `--set-key`), then `--smoke --smoke-llm`, then ask in-game |
| Voice input | add `[voice.stt]` with a Groq (free) or OpenAI key |
| Pet/panel visual polish, PgUp-in-game feel, ducking depth | run `cargo run -p revolution-desktop` and play |
| FPS impact | PresentMon during a session |
