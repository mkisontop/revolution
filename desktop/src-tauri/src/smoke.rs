//! `revolution-desktop.exe --smoke`: headless validation of every piece of
//! Windows glue that doesn't need a human — real WGC capture + hash + JPEG,
//! real WinRT TTS synthesis + playback, mic/audio-session/config probes.
//! Exit code 0 = the machine can run the companion.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::capture::CaptureShared;
use crate::{capture, config_io, duck, mic, tts};

fn section(name: &str) {
    println!("\n== {name} ==");
}

pub fn run() -> i32 {
    println!("Revolution desktop — smoke test");
    let mut failures = 0;

    // ---- config ------------------------------------------------------------
    section("config");
    let loaded = config_io::load_or_init();
    println!("path       : {}", loaded.path.display());
    println!("first run  : {}", loaded.first_run);
    println!(
        "qa         : {:?} / {} (key {})",
        loaded.cfg.roles.qa.kind,
        loaded.cfg.roles.qa.model,
        if config_io::qa_ready(&loaded.cfg) { "ready" } else { "MISSING" }
    );
    println!(
        "stt        : {}",
        if config_io::stt_ready(&loaded.cfg) { "configured" } else { "not configured (typed input fallback)" }
    );
    println!("hotkey     : {}", loaded.cfg.hotkey.ptt);
    for w in &loaded.warnings {
        println!("warning    : {w}");
    }

    // ---- capture -----------------------------------------------------------
    section("capture (WGC, primary monitor)");
    let shared = CaptureShared::new();
    shared.fresh_request.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    capture::start(shared.clone());
    let mut got = false;
    while t0.elapsed() < Duration::from_secs(5) {
        if shared.keyframes.load(Ordering::Relaxed) > 0 {
            got = true;
            break;
        }
        if let Some(e) = shared.error.lock().unwrap().clone() {
            println!("FAIL       : {e}");
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut probe_frame: Option<Vec<u8>> = None;
    if got {
        let ring = shared.ring.lock().unwrap();
        let kf = ring.latest().expect("keyframe present");
        println!(
            "OK         : first keyframe in {} ms — {} KB jpeg, dhash {:016x}",
            t0.elapsed().as_millis(),
            kf.jpeg.len() / 1024,
            kf.hash
        );
        let dims = *shared.last_dims.lock().unwrap();
        println!("frame      : {}x{}", dims.0, dims.1);
        probe_frame = Some(kf.jpeg.clone());
    } else {
        failures += 1;
        println!("FAIL       : no keyframe within 5s (RDP session or WGC unavailable?)");
    }

    // ---- tts ---------------------------------------------------------------
    section("tts (WinRT SpeechSynthesizer)");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    match tts::make_synth(&loaded.cfg.voice.tts) {
        Ok((synth, voice)) => {
            let t = Instant::now();
            match tts::synth_wav(&synth, "Revolution systems check complete.") {
                Ok(wav) => {
                    println!(
                        "OK         : voice '{voice}' — {} KB wav in {} ms",
                        wav.len() / 1024,
                        t.elapsed().as_millis()
                    );
                    match rodio::OutputStream::try_default() {
                        Ok((_stream, handle)) => match rodio::Sink::try_new(&handle) {
                            Ok(sink) => match rodio::Decoder::new(std::io::Cursor::new(wav)) {
                                Ok(src) => {
                                    sink.append(src);
                                    let t = Instant::now();
                                    while !sink.empty() && t.elapsed() < Duration::from_secs(8) {
                                        std::thread::sleep(Duration::from_millis(50));
                                    }
                                    println!("playback   : OK (you should have heard it)");
                                }
                                Err(e) => {
                                    failures += 1;
                                    println!("FAIL decode: {e}");
                                }
                            },
                            Err(e) => {
                                failures += 1;
                                println!("FAIL sink  : {e}");
                            }
                        },
                        Err(e) => {
                            failures += 1;
                            println!("FAIL output: {e}");
                        }
                    }
                }
                Err(e) => {
                    failures += 1;
                    println!("FAIL synth : {e}");
                }
            }
        }
        Err(e) => {
            failures += 1;
            println!("FAIL       : {e}");
        }
    }

    // ---- chat-audio tts (when configured) ----------------------------------
    if let Some(api) = tts::api_from_cfg(&loaded.cfg.voice.tts) {
        section("tts (chat-audio natural voice)");
        println!("engine     : {} — voice '{}'", api.model, api.voice);
        let client = reqwest::Client::new();
        let t = Instant::now();
        let mut pcm: Vec<i16> = Vec::new();
        match crate::llm::rt().block_on(tts::stream_tts_pcm(
            &client,
            &api,
            "The natural voice is online and ready to go.",
            |s| {
                pcm.extend(s);
                true
            },
        )) {
            Ok(_) => {
                println!(
                    "OK         : {} KB pcm in {} ms",
                    pcm.len() * 2 / 1024,
                    t.elapsed().as_millis()
                );
                if let Ok((_stream, handle)) = rodio::OutputStream::try_default() {
                    if let Ok(sk) = rodio::Sink::try_new(&handle) {
                        sk.append(rodio::buffer::SamplesBuffer::new(1, tts::API_TTS_RATE, pcm));
                        let t = Instant::now();
                        while !sk.empty() && t.elapsed() < Duration::from_secs(12) {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        println!("playback   : OK (you should have heard the natural voice)");
                    }
                }
            }
            Err(e) => {
                failures += 1;
                println!("FAIL       : {e:#}");
            }
        }
    }

    // ---- mic ---------------------------------------------------------------
    section("mic (cpal/WASAPI)");
    let m = mic::spawn();
    std::thread::sleep(Duration::from_millis(600));
    println!("{}", m.status.lock().unwrap());

    // ---- audio sessions (ducking plumbing) ---------------------------------
    section("audio sessions");
    match duck::probe_session_count() {
        Ok(n) => println!("OK         : {n} other render session(s) visible"),
        Err(e) => println!("warn       : {e} (ducking may not work)"),
    }

    // ---- voice input loopback (opt-in: --smoke-stt) ------------------------
    if std::env::args().any(|a| a == "--smoke-stt") {
        section("voice input loopback (--smoke-stt)");
        match loaded.cfg.voice.stt.clone().filter(|_| config_io::stt_ready(&loaded.cfg)) {
            Some(stt_cfg) => {
                // Synthesize a spoken question, then run it through the real
                // transcription path — no human mic needed.
                let spoken = "Which pal is best for mining coal in Palworld?";
                match tts::make_synth(&loaded.cfg.voice.tts)
                    .and_then(|(s, _)| tts::synth_wav(&s, spoken))
                {
                    Ok(wav) => {
                        println!("stt        : {:?} / {}", stt_cfg.kind, stt_cfg.model);
                        println!("spoken     : \"{spoken}\" ({} KB wav)", wav.len() / 1024);
                        let client = reqwest::Client::new();
                        let t = Instant::now();
                        match crate::llm::rt()
                            .block_on(crate::stt::transcribe(&client, &stt_cfg, wav, Some("Palworld")))
                        {
                            Ok(text) => {
                                println!("transcript : \"{text}\" in {} ms", t.elapsed().as_millis());
                                if text.to_lowercase().contains("coal") {
                                    println!("OK         : transcript matches");
                                } else {
                                    failures += 1;
                                    println!("FAIL       : transcript does not match the spoken text");
                                }
                            }
                            Err(e) => {
                                failures += 1;
                                println!("FAIL       : {e:#}");
                            }
                        }
                    }
                    Err(e) => {
                        failures += 1;
                        println!("FAIL synth : {e}");
                    }
                }
            }
            None => println!("skip       : [voice.stt] not configured or key missing"),
        }
    }

    // ---- live LLM lane (opt-in: --smoke-llm) -------------------------------
    if std::env::args().any(|a| a == "--smoke-llm") {
        section("live model lane (--smoke-llm)");
        if config_io::qa_ready(&loaded.cfg) {
            if !llm_probe(&loaded.cfg, probe_frame) {
                failures += 1;
            }
        } else {
            println!("skip       : no usable roles.qa key");
        }
    }

    println!(
        "\nsmoke result: {}",
        if failures == 0 { "PASS" } else { "FAIL" }
    );
    i32::from(failures > 0)
}

/// One tiny live streamed request through the production transport
/// (`llm::spawn_stream`) — proves endpoint, auth, SSE parsing, and measures
/// real TTFT. When a captured keyframe is available it rides along, proving
/// the capture→encode→vision chain live. Web search disabled; small cap.
fn llm_probe(cfg: &revolution_core::config::AppConfig, frame: Option<Vec<u8>>) -> bool {
    use base64::Engine;
    use revolution_core::orchestrator::InputEvent;
    use revolution_core::providers::{ChatRequest, ImageAttachment, Turn};

    use crate::msg::LoopMsg;
    use crate::util::now_ms;

    let mut qa = cfg.roles.qa.clone();
    qa.enable_web_search = false;
    qa.max_output_tokens = 80;
    println!("endpoint   : {:?} / {}", qa.kind, qa.model);

    let turn = match frame {
        Some(jpeg) => {
            println!("vision     : attaching the live keyframe ({} KB)", jpeg.len() / 1024);
            Turn::User {
                text: "In one short sentence, what is on this screen?".into(),
                images: vec![ImageAttachment {
                    media_type: "image/jpeg".into(),
                    base64_data: base64::engine::general_purpose::STANDARD.encode(&jpeg),
                }],
            }
        }
        None => Turn::User { text: "ping — reply with the single word: check".into(), images: Vec::new() },
    };
    let req = ChatRequest {
        system: "You are a terse connectivity probe. Answer in at most one sentence.".into(),
        context_block: String::new(),
        turns: vec![turn],
    };

    let (ltx, lrx) = std::sync::mpsc::channel::<LoopMsg>();
    let (ttx, _trx_keepalive) = std::sync::mpsc::channel();
    let t0 = now_ms();
    let _handle = crate::llm::spawn_stream(reqwest::Client::new(), qa, req, ltx, ttx);

    let mut text = String::new();
    loop {
        match lrx.recv_timeout(Duration::from_secs(30)) {
            Ok(LoopMsg::Input(InputEvent::FirstToken { ms })) => {
                println!("TTFT       : {} ms", ms.saturating_sub(t0));
            }
            Ok(LoopMsg::Delta(t)) => text.push_str(&t),
            Ok(LoopMsg::ToolActivity(w)) => println!("tool       : {w}"),
            Ok(LoopMsg::LlmDone { usage, .. }) => {
                println!("total      : {} ms", now_ms().saturating_sub(t0));
                println!("reply      : {}", text.trim());
                if let Some(u) = usage {
                    println!(
                        "usage      : {} in / {} out ({} cached)",
                        u.input_tokens, u.output_tokens, u.cache_read_tokens
                    );
                }
                return true;
            }
            Ok(LoopMsg::LlmFailed(e)) => {
                println!("FAIL       : {e}");
                return false;
            }
            Ok(_) => {}
            Err(_) => {
                println!("FAIL       : no response within 30s");
                return false;
            }
        }
    }
}
