# Revolution desktop shell (Windows 11) — bring-up guide

The Tauri v2 shell is intentionally **not** in the cargo workspace yet: it depends on Windows-only crates and WebView2, so it can't build in the Linux container that authored this repo. Everything below wires real OS inputs into `revolution-core`, whose interfaces are already tested (see `docs/VALIDATION.md`).

## Prerequisites (on the Windows machine)

- Rust (stable) + `cargo install create-tauri-app tauri-cli`
- Node 20+ (UI dev server), WebView2 runtime (ships with Win11)

## Scaffold

```powershell
# from the repo root
cargo create-tauri-app desktop --template vanilla-ts   # or svelte/react
# add to the workspace: edit /Cargo.toml members += "desktop/src-tauri"
cargo add --package desktop revolution-core --path ../core
```

## Crate wiring map (core API ↔ OS glue)

| Shell component | Windows tech | Feeds / consumes in `revolution-core` |
|---|---|---|
| Capture service | `windows-capture` crate (WGC) targeting the game HWND; GPU downscale; libjpeg-turbo (`turbojpeg` crate) | build `GrayThumb` thumbnails → `dhash64` → `ChangeGate::decide` → `KeyframeRing::push` |
| Hotkey | `WH_KEYBOARD_LL` hook via `rdev` (dedicated thread); Tauri global-shortcut as first attempt | `Orchestrator::handle(PttDown/PttUp)` → execute returned `Action`s |
| Mic + playback | `cpal` (WASAPI); duck game session via `IAudioSessionManager2`/`ISimpleAudioVolume` (`windows` crate) on `Action::DuckGameAudio` | STT websocket frames out; TTS audio in; `TtsFirstAudio`/`PlaybackFinished` events back |
| STT/TTS clients | `tokio-tungstenite` websockets (Deepgram / Cartesia or ElevenLabs) | `SttFinal` event; sentence-chunker feeds TTS from `StreamEvent::TextDelta` |
| LLM transport | `reqwest` streaming | execute `HttpRequestSpec` from `provider_for(kind).build_request(...)`; pipe body chunks into `SseAssembler` + `parse_sse` |
| Game detector | `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` → exe path; Discord detectable DB (cache JSON); Steam library scan | `match_exe`, `parse_appmanifest`, `exe_in_installdir` → `MemoryStore::get_or_create_game` |
| Memory | app-data SQLite file; `fastembed` (bge-small int8) implementing the `Embedder` trait | `MemoryStore::open(path, embedder)` |
| Pet + panel windows | Tauri: `transparent: true`, `decorations: false`, `alwaysOnTop: true`, `skipTaskbar: true`; ~60Hz cursor poll toggling `set_ignore_cursor_events`; `WS_EX_NOACTIVATE` | render `Action::SetPet(state)`; show `TextDelta` in the speech bubble |
| Config/settings | `config.toml` in app-data; API keys in Windows Credential Manager (`keyring` crate; `keyring:` refs in the TOML) | `AppConfig::from_toml` / `AppConfig::example_toml()` |

## Suggested window config (tauri.conf.json fragment)

```json
{
  "app": {
    "windows": [
      { "label": "pet",   "transparent": true, "decorations": false, "alwaysOnTop": true,
        "skipTaskbar": true, "shadow": false, "width": 200, "height": 200, "resizable": false },
      { "label": "panel", "transparent": true, "decorations": false, "alwaysOnTop": true,
        "skipTaskbar": true, "visible": false, "width": 420, "height": 640 }
    ]
  }
}
```

## Phase 1 exit test (from docs/ROADMAP.md)

Run a game in borderless windowed, hold PgUp, ask a question about what's on screen, release: correct spoken answer < 2s p50 (debug overlay shows the per-stage ledger from `LatencyMarks::report()`), and PresentMon shows no measurable FPS delta with the app running.
