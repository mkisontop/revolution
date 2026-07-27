//! End-to-end simulation of one full companion turn, wiring every module
//! together the way the Windows shell will: capture → change gate → ring →
//! PTT orchestrator → memory retrieval → prompt build → provider request →
//! streamed reply → latency report. No network, no OS — pure logic.

use revolution_core::config::AppConfig;
use revolution_core::memory::embed::HashEmbedder;
use revolution_core::memory::store::MemoryStore;
use revolution_core::orchestrator::{Action, InputEvent, Orchestrator};
use revolution_core::prompt::{default_system_core, PromptBuilder, PromptConfig, Transcript};
use revolution_core::providers::sse::SseAssembler;
use revolution_core::providers::{provider_for, ImageAttachment, StreamEvent, Turn};
use revolution_core::vision::dhash::{dhash64, GrayThumb};
use revolution_core::vision::gate::{ChangeGate, GateConfig, GateDecision};
use revolution_core::vision::ring::{Keyframe, KeyframeRing};

/// Synthetic "game scene": a gradient whose phase encodes the scene state,
/// so scene changes register on the perceptual hash like real ones do.
fn scene(phase: u32) -> GrayThumb {
    GrayThumb::from_fn(320, 180, move |x, y| {
        (((x * 2 + y + phase * 37) % 256) as u8).wrapping_add((phase * 11) as u8)
    })
}

#[test]
fn full_ptt_turn_across_all_modules() {
    // ---- 0. User config (Grok via the OpenAI-compatible adapter) ----------
    let cfg = AppConfig::from_toml(
        r#"
        [roles.qa]
        kind = "openai_compat"
        base_url = "https://api.x.ai/v1"
        model = "grok-vision-fast"
        api_key = "XAI_KEY"
        enable_web_search = true
        max_output_tokens = 400
        "#,
    )
    .expect("config parses");

    // ---- 1. Ambient watching: frames flow through gate into the ring ------
    let mut gate = ChangeGate::new(GateConfig::default());
    let mut ring = KeyframeRing::new(60, 30 << 20);
    let mut clock: u64 = 0;
    let mut promoted = 0u32;
    for tick in 0..30u32 {
        clock += 2_000;
        // Scene changes every 10 ticks; otherwise static (gate should drop).
        let phase = tick / 10;
        let hash = dhash64(&scene(phase));
        match gate.decide(clock, hash) {
            GateDecision::Promote(reason) => {
                promoted += 1;
                ring.push(Keyframe {
                    ts_ms: clock,
                    hash,
                    jpeg: vec![0xFF; 150_000], // stand-in for an encoded frame
                    ocr_text: None,
                    change_score: match reason {
                        revolution_core::vision::gate::PromoteReason::Changed { distance } => distance,
                        _ => 0,
                    },
                });
            }
            GateDecision::Drop => {}
        }
    }
    assert!(
        (3..=8).contains(&promoted),
        "change-gating should keep keyframes sparse; promoted {promoted} of 30"
    );

    // ---- 2. Long-term memory already knows this player --------------------
    let mut store = MemoryStore::open_in_memory(Box::new(HashEmbedder::default())).unwrap();
    let game = store.get_or_create_game("Palworld", Some("palworld.exe")).unwrap();
    store.set_profile(game, "Level 29. Volcano-side base. Hates spoilers.", 1).unwrap();
    store.add_episode(game, 0, 1, "Last session: caught Anubis, started coal farm.").unwrap();
    store.remember(game, "Anubis is the player's strongest fighting pal", "note", 2).unwrap();
    store.remember(game, "base is short on coal for weapon production", "goal", 3).unwrap();

    // ---- 3. The player holds PgUp and asks ---------------------------------
    let mut orch = Orchestrator::new();
    let t0 = clock + 500;
    let actions = orch.handle(InputEvent::PttDown { ms: t0 });
    assert!(actions.contains(&Action::CaptureFreshFrame));
    assert!(actions.contains(&Action::StartSttStream));

    // Fresh key-down frame lands in the ring while the player is talking.
    let fresh_hash = dhash64(&scene(99));
    ring.push(Keyframe { ts_ms: t0 + 30, hash: fresh_hash, jpeg: vec![0xEE; 140_000], ocr_text: None, change_score: 25 });

    let t_up = t0 + 2_600;
    orch.handle(InputEvent::PttUp { ms: t_up });
    let question = "which pal should I use for the coal problem?";
    let actions = orch.handle(InputEvent::SttFinal { ms: t_up + 220, text: question.into() });
    assert_eq!(actions, vec![Action::SendChatRequest { question: question.into() }]);

    // ---- 4. Build the prompt: profile + memories + frames ------------------
    let frames: Vec<ImageAttachment> = ring
        .select_for_question(3, 5_000)
        .into_iter()
        .map(|kf| ImageAttachment { media_type: "image/jpeg".into(), base64_data: format!("<{}b>", kf.jpeg.len()) })
        .collect();
    assert!(!frames.is_empty() && frames.len() <= 3);

    let memories: Vec<String> =
        store.retrieve(game, question, 3).unwrap().into_iter().map(|f| f.text).collect();
    assert!(memories.iter().any(|m| m.contains("coal")), "retrieval should surface the coal goal");

    let builder = PromptBuilder::new(default_system_core());
    let pcfg = PromptConfig::default();
    let transcript = Transcript::new();
    let profile = store.get_profile(game).unwrap().unwrap();
    let episode = store.latest_episode(game).unwrap();
    let req = builder.build(
        &pcfg,
        Some("Monday, July 28 2026"),
        &profile,
        episode.as_deref(),
        &memories,
        &transcript,
        question,
        frames,
    );
    assert!(req.context_block.contains("Monday, July 28 2026"));

    assert!(req.context_block.contains("Volcano-side base"));
    assert!(req.context_block.contains("caught Anubis"));
    let Turn::User { images, .. } = req.turns.last().unwrap() else { panic!("last turn is the question") };
    assert!(!images.is_empty());

    // ---- 5. Provider request for the user's configured endpoint ------------
    let mut provider = provider_for(cfg.roles.qa.kind);
    let spec = provider.build_request(&cfg.roles.qa, &req);
    assert_eq!(spec.url, "https://api.x.ai/v1/chat/completions");
    assert_eq!(spec.body["search_parameters"]["mode"], "auto"); // Grok Live Search on
    assert_eq!(spec.body["model"], "grok-vision-fast");

    // ---- 6. The streamed reply comes back (canned OpenAI-format SSE) -------
    let mut sse = SseAssembler::new();
    let raw = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Digtoise is your coal fix — \"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"park it on the ore nodes by your base.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4200,\"completion_tokens\":22,\"prompt_tokens_details\":{\"cached_tokens\":3100}}}\n\n",
        "data: [DONE]\n\n",
    );
    let mut text = String::new();
    let mut usage_seen = false;
    let mut done = false;
    // Split at an awkward boundary to prove incremental assembly works.
    for chunk in [&raw[..97], &raw[97..]] {
        for msg in sse.push(chunk) {
            for ev in provider.parse_sse(&msg) {
                match ev {
                    StreamEvent::TextDelta(t) => text.push_str(&t),
                    StreamEvent::Usage { cache_read_tokens, .. } => {
                        usage_seen = true;
                        assert_eq!(cache_read_tokens, 3_100);
                    }
                    StreamEvent::Done { stop_reason } => {
                        done = true;
                        assert_eq!(stop_reason.as_deref(), Some("stop"));
                    }
                    StreamEvent::ToolActivity(_) => {}
                }
            }
        }
    }
    assert!(done && usage_seen);
    assert_eq!(text, "Digtoise is your coal fix — park it on the ore nodes by your base.");

    // ---- 7. Voice timeline closes within budget ----------------------------
    orch.handle(InputEvent::FirstToken { ms: t_up + 900 });
    orch.handle(InputEvent::TtsFirstAudio { ms: t_up + 1_250 });
    let report = orch.marks.report().expect("full marks");
    assert_eq!(report.release_to_first_audio_ms, 1_250);
    assert!(report.within_budget, "sim turn must sit inside the 2s budget: {report:?}");

    // ---- 8. The model remembered something new; the gate dedupes it --------
    let d1 = store.remember(game, "Digtoise assigned to mining coal at the base", "note", 10).unwrap();
    let d2 = store.remember(game, "digtoise assigned to mining coal at the base", "note", 11).unwrap();
    use revolution_core::memory::store::FactGateDecision;
    assert_eq!(d1, FactGateDecision::Add);
    assert_eq!(d2, FactGateDecision::Noop);
}
