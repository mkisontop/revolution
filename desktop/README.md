# Revolution desktop shell (Windows 11)

The Tauri v2 shell is **implemented** (Phase 1 bring-up). It wires real OS
inputs into the tested `revolution-core` engine:

```
desktop/
├── ui/                  # static frontend (no bundler): pet overlay + panel
│   ├── index.html       #   the pet: state animations, speech bubble, capture dot
│   └── panel.html       #   transcript, typed asks, latency ledger, keys, config
└── src-tauri/
    ├── tauri.conf.json  # pet (transparent/always-on-top) + panel windows
    └── src/
        ├── main.rs      # entrypoint, tauri commands, --smoke mode
        ├── runloop.rs   # THE loop: owns Orchestrator/Transcript/MemoryStore,
        │                #   executes Actions, mirrors state to the UI
        ├── capture.rs   # WGC monitor capture → luma dHash → ChangeGate →
        │                #   JPEG → KeyframeRing (RAM only)
        ├── hotkey.rs    # WH_KEYBOARD_LL push-to-talk hook (PgUp default)
        ├── mic.rs       # cpal/WASAPI always-open input, retained while PTT held
        ├── tts.rs       # WinRT SpeechSynthesizer (zero-key) → rodio sink,
        │                #   sentence streaming + barge-in
        ├── stt.rs       # batch Whisper via any OpenAI-compatible endpoint
        ├── llm.rs       # reqwest streaming of core's HttpRequestSpec → SSE →
        │                #   normalized events; sentence chunker feeds TTS
        ├── duck.rs      # IAudioSessionManager2 per-app ducking (25%) + restore
        ├── gamewatch.rs # foreground exe → Steam manifests / detectable DB ladder
        ├── config_io.rs # %APPDATA%\revolution\config.toml + Credential Manager
        └── smoke.rs     # --smoke: headless hardware validation
```

## Run it

```powershell
# from the repo root — dev build, real windows
cargo run -p revolution-desktop

# headless hardware check first (capture, TTS voice, mic, audio sessions):
cargo run -p revolution-desktop -- --smoke
```

First launch writes the annotated example config to
`%APPDATA%\revolution\config.toml` and shows the pet bottom-right:

1. **Click the pet** → panel opens.
2. Paste your model key under **API keys** (stored in Windows Credential
   Manager, never on disk) — the example config's `roles.qa` block points at
   xAI Grok; switch the block for Gemini/OpenAI/Claude per the comments.
3. Restart the app. Type a question in the panel — you should hear the answer
   through the built-in Windows voice (zero extra keys needed).
4. For voice *input*, add `[voice.stt]` (Groq's free Whisper tier works) and
   hold **PgUp** to talk.

## What the shell guarantees

- **Privacy**: frames live only in the RAM ring; they leave the machine only
  when you ask a question (`upload_on_ask_only`). With
  `game_foreground_only = true` (default) capture pauses whenever the
  foreground app isn't a detected game. The pet + panel windows are excluded
  from capture (`WDA_EXCLUDEFROMCAPTURE`), so Rev never sees itself.
- **Latency ledger**: every voice turn emits release→first-audio timing
  split into STT / LLM-TTFT / TTS, shown in the panel against the 2 s budget.
- **Barge-in**: PgUp while Rev is speaking stops playback, aborts the
  in-flight stream, and starts listening again.
- **Zero-key floor**: with no keys at all the app still runs — typed
  questions + Windows TTS; each key you add upgrades a stage (Whisper STT,
  better models, later streaming voices).

## Not yet wired (Phase 2+)

- Session-end summarizer + `remember`/`update_profile` model tools (the
  panel's manual "Remember something" box writes the same store today).
- fastembed embeddings (deterministic `HashEmbedder` stands in — retrieval
  is keyword-dominant until then).
- Discord detectable DB auto-download (drop a `detectable.json` into
  `%APPDATA%\revolution` to enable that ladder step today), Deepgram
  streaming STT, ElevenLabs/Cartesia TTS, hype mode, deep-research lane.
