//! Revolution desktop shell entrypoint: `--smoke` for headless hardware
//! validation, otherwise the Tauri pet + panel with every service thread,
//! a system tray, and the panel's command surface (settings, memory
//! browser, diagnostics).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod config_io;
mod duck;
mod gamewatch;
mod hotkey;
mod llm;
mod mic;
mod msg;
mod runloop;
mod smoke;
mod stt;
mod tts;
mod util;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, State};

use crate::capture::CaptureShared;
use crate::msg::{LoopMsg, TtsCmd, UiQuery};
use crate::runloop::Status;

struct PanelState {
    loop_tx: Mutex<Sender<LoopMsg>>,
    tts_tx: Mutex<Sender<TtsCmd>>,
    status: Arc<Mutex<Status>>,
    muted: Arc<AtomicBool>,
}

/// Round-trip a query to the run-loop thread (which owns the memory store).
fn ask_loop<T>(
    state: &State<PanelState>,
    make: impl FnOnce(std::sync::mpsc::Sender<T>) -> UiQuery,
) -> Result<T, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    state
        .loop_tx
        .lock()
        .unwrap()
        .send(LoopMsg::Query(make(tx)))
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(3)).map_err(|e| e.to_string())
}

#[tauri::command]
fn send_typed(state: State<PanelState>, text: String) {
    let _ = state.loop_tx.lock().unwrap().send(LoopMsg::Typed(text));
}

#[tauri::command]
fn remember_note(state: State<PanelState>, text: String) {
    let _ = state.loop_tx.lock().unwrap().send(LoopMsg::RememberNote(text));
}

#[tauri::command]
fn get_status(state: State<PanelState>) -> Status {
    state.status.lock().unwrap().clone()
}

#[tauri::command]
fn set_muted(state: State<PanelState>, muted: bool) {
    let _ = state.loop_tx.lock().unwrap().send(LoopMsg::SetMuted(muted));
}

#[tauri::command]
fn get_config_text() -> Result<String, String> {
    std::fs::read_to_string(config_io::config_path()).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_config_text(text: String) -> Result<(), String> {
    revolution_core::config::AppConfig::from_toml(&text).map_err(|e| e.to_string())?;
    std::fs::write(config_io::config_path(), text).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_secret(name: String, value: String) -> Result<(), String> {
    config_io::save_secret(name.trim(), value.trim()).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_config_dir() -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(config_io::config_dir())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn toggle_panel(app: tauri::AppHandle) {
    if let Some(panel) = app.get_webview_window("panel") {
        let visible = panel.is_visible().unwrap_or(false);
        if visible {
            let _ = panel.hide();
        } else {
            let _ = panel.show();
            let _ = panel.set_focus();
        }
    }
}

// ---- memory browser ---------------------------------------------------------

#[tauri::command]
fn list_games(state: State<PanelState>) -> Result<serde_json::Value, String> {
    let rows = ask_loop(&state, UiQuery::Games)?;
    Ok(serde_json::json!(rows
        .into_iter()
        .map(|g| serde_json::json!({"id": g.id, "name": g.name, "facts": g.fact_count}))
        .collect::<Vec<_>>()))
}

#[tauri::command]
fn list_facts(state: State<PanelState>, game_id: i64) -> Result<serde_json::Value, String> {
    let rows = ask_loop(&state, |reply| UiQuery::Facts { game_id, reply })?;
    Ok(serde_json::json!(rows
        .into_iter()
        .map(|f| serde_json::json!({
            "id": f.id, "kind": f.kind, "text": f.text, "updated_at": f.updated_at
        }))
        .collect::<Vec<_>>()))
}

#[tauri::command]
fn forget_fact(state: State<PanelState>, fact_id: i64) -> Result<bool, String> {
    ask_loop(&state, |reply| UiQuery::Forget { fact_id, reply })
}

#[tauri::command]
fn get_profile(state: State<PanelState>, game_id: i64) -> Result<String, String> {
    ask_loop(&state, |reply| UiQuery::Profile { game_id, reply })
}

#[tauri::command]
fn set_profile(state: State<PanelState>, game_id: i64, markdown: String) -> Result<bool, String> {
    ask_loop(&state, |reply| UiQuery::SetProfile { game_id, markdown, reply })
}

#[tauri::command]
fn get_usage(state: State<PanelState>) -> Result<msg::UsageSnapshot, String> {
    ask_loop(&state, UiQuery::Usage)
}

// ---- settings ---------------------------------------------------------------

/// Non-secret view of the on-disk config for the settings forms, plus which
/// key slots have a secret stored.
#[tauri::command]
fn get_settings() -> Result<serde_json::Value, String> {
    let text =
        std::fs::read_to_string(config_io::config_path()).map_err(|e| e.to_string())?;
    let cfg = revolution_core::config::AppConfig::from_toml(&text).map_err(|e| e.to_string())?;
    let q = &cfg.roles.qa;
    Ok(serde_json::json!({
        "hotkey": { "ptt": cfg.hotkey.ptt, "toggle_mode": cfg.hotkey.toggle_mode },
        "qa": {
            "kind": format!("{:?}", q.kind),
            "base_url": q.base_url,
            "model": q.model,
            "effort": q.effort,
            "enable_web_search": q.enable_web_search,
            "max_output_tokens": q.max_output_tokens,
            "api_key_ref": q.api_key,
        },
        "search": cfg.roles.search.as_ref().map(|s| serde_json::json!({
            "base_url": s.base_url, "model": s.model,
        })),
        "stt": cfg.voice.stt.as_ref().map(|s| serde_json::json!({
            "kind": format!("{:?}", s.kind), "base_url": s.base_url, "model": s.model,
        })),
        "tts": {
            "engine": format!("{:?}", cfg.voice.tts.engine),
            "voice": cfg.voice.tts.voice,
            "rate": cfg.voice.tts.rate,
            "model": cfg.voice.tts.model,
            "base_url": cfg.voice.tts.base_url,
        },
        "privacy": {
            "upload_on_ask_only": cfg.privacy.upload_on_ask_only,
            "hype_mode": cfg.privacy.hype_mode,
            "capture_indicator": cfg.privacy.capture_indicator,
            "game_foreground_only": cfg.privacy.game_foreground_only,
        },
        "companion": {
            "greet_on_game": cfg.companion.greet_on_game,
            "ambient_min_gap_secs": cfg.companion.ambient_min_gap_secs,
            "sleep_after_secs": cfg.companion.sleep_after_secs,
        },
        "budget": {
            "monthly_usd_cap": cfg.budget.monthly_usd_cap,
            "usd_per_1m_input": cfg.budget.usd_per_1m_input,
            "usd_per_1m_output": cfg.budget.usd_per_1m_output,
        },
        "keys": {
            "qa": config_io::secret_exists("revolution/qa"),
            "stt": config_io::secret_exists("revolution/stt"),
            "ninerouter": config_io::secret_exists("revolution/9router"),
        },
    }))
}

fn json_to_toml_value(v: &serde_json::Value) -> Result<toml_edit::Value, String> {
    match v {
        serde_json::Value::Bool(b) => Ok((*b).into()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.into())
            } else if let Some(f) = n.as_f64() {
                Ok(f.into())
            } else {
                Err("unsupported number".into())
            }
        }
        serde_json::Value::String(s) => Ok(s.as_str().into()),
        other => Err(format!("unsupported value: {other}")),
    }
}

/// Surgical config patch from the settings forms. Shape:
/// `{"set": {"roles.qa.model": "x", ...}, "remove": ["voice.stt", ...],
///   "tables": {"voice.stt": {"kind": "...", ...}, ...}}`
/// Scalars are edited in place (comments survive); `tables` replaces whole
/// blocks; everything is validated as a full AppConfig before writing.
#[tauri::command]
fn apply_settings(patch: serde_json::Value) -> Result<(), String> {
    let text =
        std::fs::read_to_string(config_io::config_path()).map_err(|e| e.to_string())?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("parse: {e}"))?;

    fn nav<'a>(
        doc: &'a mut toml_edit::DocumentMut,
        parts: &[&str],
    ) -> &'a mut toml_edit::Item {
        let mut item = doc.as_item_mut();
        for p in parts {
            if item.get(p).is_none() {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                item[p] = toml_edit::Item::Table(t);
            }
            item = &mut item[p];
        }
        item
    }

    if let Some(sets) = patch.get("set").and_then(|v| v.as_object()) {
        for (path, val) in sets {
            let parts: Vec<&str> = path.split('.').collect();
            let (last, parents) = parts.split_last().ok_or("empty path")?;
            let item = nav(&mut doc, parents);
            item[last] = toml_edit::value(json_to_toml_value(val)?);
        }
    }
    if let Some(removes) = patch.get("remove").and_then(|v| v.as_array()) {
        for path in removes.iter().filter_map(|p| p.as_str()) {
            let parts: Vec<&str> = path.split('.').collect();
            let (last, parents) = parts.split_last().ok_or("empty path")?;
            let item = nav(&mut doc, parents);
            if let Some(t) = item.as_table_mut() {
                t.remove(last);
            }
        }
    }
    if let Some(tables) = patch.get("tables").and_then(|v| v.as_object()) {
        for (path, obj) in tables {
            let parts: Vec<&str> = path.split('.').collect();
            let obj = obj.as_object().ok_or("table patch must be an object")?;
            let mut t = toml_edit::Table::new();
            for (k, v) in obj {
                t[k.as_str()] = toml_edit::value(json_to_toml_value(v)?);
            }
            let (last, parents) = parts.split_last().ok_or("empty path")?;
            let item = nav(&mut doc, parents);
            item[last] = toml_edit::Item::Table(t);
        }
    }

    let out = doc.to_string();
    revolution_core::config::AppConfig::from_toml(&out)
        .map_err(|e| format!("would produce an invalid config: {e}"))?;
    std::fs::write(config_io::config_path(), out).map_err(|e| e.to_string())
}

/// Live connectivity probe of the on-disk brain config (fresh load, so it
/// tests what the user just saved — no restart needed to find out).
#[tauri::command]
fn test_brain() -> Result<String, String> {
    let loaded = config_io::load_or_init();
    if !config_io::qa_ready(&loaded.cfg) {
        return Err("No usable API key for the brain yet — store one first.".into());
    }
    let mut cfg = loaded.cfg.roles.qa.clone();
    cfg.max_output_tokens = 16;
    cfg.enable_web_search = false;
    let req = revolution_core::providers::ChatRequest {
        system: "You are a connectivity probe. Reply with exactly the single word: online"
            .into(),
        context_block: String::new(),
        turns: vec![revolution_core::providers::Turn::User {
            text: "ping".into(),
            images: vec![],
        }],
    };
    let client = reqwest::Client::new();
    let started = std::time::Instant::now();
    let out = llm::rt()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(25), llm::collect_stream(&client, &cfg, &req))
                .await
        })
        .map_err(|_| "Timed out after 25 s".to_string())?
        .map_err(|e| format!("{e:#}"))?;
    let ms = started.elapsed().as_millis();
    let snippet: String = out.chars().take(60).collect();
    Ok(format!("{snippet} — {ms} ms"))
}

#[tauri::command]
fn tts_test(state: State<PanelState>) {
    let _ = state
        .tts_tx
        .lock()
        .unwrap()
        .send(TtsCmd::Sentence("Hey! This is what I sound like.".into()));
    let _ = state.tts_tx.lock().unwrap().send(TtsCmd::EndOfUtterance);
}

#[tauri::command]
fn start_router() {
    util::launch_9router();
}

// ---- autostart --------------------------------------------------------------

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

#[tauri::command]
fn get_autostart() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>("Revolution"))
        .is_ok()
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<(), String> {
    let key = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, winreg::enums::KEY_ALL_ACCESS)
        .map_err(|e| e.to_string())?;
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        key.set_value("Revolution", &format!("\"{}\"", exe.display()))
            .map_err(|e| e.to_string())
    } else {
        match key.delete_value("Revolution") {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[tauri::command]
fn restart_app(app: tauri::AppHandle) {
    app.restart();
}

#[tauri::command]
fn quit_app(state: State<PanelState>) {
    let _ = state.loop_tx.lock().unwrap().send(LoopMsg::Shutdown);
}

/// Hide a window from WGC/DXGI capture so the pet never appears in its own
/// screenshots (`WDA_EXCLUDEFROMCAPTURE`, Win10 2004+).
fn exclude_from_capture(window: &tauri::WebviewWindow) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
    };
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            let _ = SetWindowDisplayAffinity(
                windows::Win32::Foundation::HWND(hwnd.0 as _),
                WDA_EXCLUDEFROMCAPTURE,
            );
        }
    }
}

fn main() {
    util::init_clock();
    let args: Vec<String> = std::env::args().collect();
    // Store a secret without the GUI: echo KEY | revolution-desktop --set-key revolution/qa
    if let Some(pos) = args.iter().position(|a| a == "--set-key") {
        let Some(name) = args.get(pos + 1) else {
            eprintln!("usage: revolution-desktop --set-key <name>   (key on stdin)");
            std::process::exit(2);
        };
        let mut value = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut value);
        match config_io::save_secret(name, value.trim()) {
            Ok(()) => {
                println!("stored keyring:{name}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("failed: {e}");
                std::process::exit(1);
            }
        }
    }
    if args.iter().any(|a| a == "--smoke") {
        std::process::exit(smoke::run());
    }

    let loaded = config_io::load_or_init();
    let (loop_tx, loop_rx) = std::sync::mpsc::channel::<LoopMsg>();
    let capture_shared = CaptureShared::new();
    let muted = Arc::new(AtomicBool::new(false));

    let mut initial_status = Status {
        config_path: loaded.path.to_string_lossy().to_string(),
        warnings: loaded.warnings.clone(),
        first_run: loaded.first_run,
        ..Status::default()
    };
    if let Some(w) = hotkey::spawn(&loaded.cfg.hotkey.ptt, loop_tx.clone()) {
        initial_status.warnings.push(w);
    }
    let status = Arc::new(Mutex::new(initial_status));

    let mic = mic::spawn();
    let tts_tx = tts::spawn(loaded.cfg.voice.tts.clone(), loop_tx.clone(), muted.clone());
    capture::start(capture_shared.clone());
    gamewatch::spawn(loop_tx.clone(), config_io::config_dir());

    let panel_state = PanelState {
        loop_tx: Mutex::new(loop_tx.clone()),
        tts_tx: Mutex::new(tts_tx.clone()),
        status: status.clone(),
        muted: muted.clone(),
    };

    tauri::Builder::default()
        .manage(panel_state)
        .invoke_handler(tauri::generate_handler![
            send_typed,
            remember_note,
            get_status,
            set_muted,
            get_config_text,
            save_config_text,
            save_secret,
            open_config_dir,
            toggle_panel,
            list_games,
            list_facts,
            forget_fact,
            get_profile,
            set_profile,
            get_usage,
            get_settings,
            apply_settings,
            test_brain,
            tts_test,
            start_router,
            get_autostart,
            set_autostart,
            restart_app,
            quit_app
        ])
        .setup(move |app| {
            // Pet bottom-right of the primary work area; both windows
            // invisible to capture.
            if let Some(pet) = app.get_webview_window("pet") {
                exclude_from_capture(&pet);
                if let Ok(Some(monitor)) = pet.primary_monitor() {
                    let m = monitor.size();
                    let w = pet.outer_size().map(|s| s.width).unwrap_or(280);
                    let h = pet.outer_size().map(|s| s.height).unwrap_or(340);
                    let x = m.width.saturating_sub(w + 24) as i32;
                    let y = m.height.saturating_sub(h + 96) as i32;
                    let _ = pet.set_position(tauri::PhysicalPosition::new(x, y));
                }
            }
            if let Some(panel) = app.get_webview_window("panel") {
                exclude_from_capture(&panel);
            }

            // System tray: the app's home base (the pet has no taskbar
            // presence, so this is also the only always-there quit).
            let mute_item = CheckMenuItemBuilder::with_id("mute", "Mute Rev")
                .checked(false)
                .build(app)?;
            let autostart_item =
                CheckMenuItemBuilder::with_id("autostart", "Start with Windows")
                    .checked(get_autostart())
                    .build(app)?;
            let menu = MenuBuilder::new(app)
                .item(&MenuItemBuilder::with_id("panel", "Open Rev's panel").build(app)?)
                .item(&mute_item)
                .item(&autostart_item)
                .separator()
                .item(&MenuItemBuilder::with_id("quit", "Quit Rev").build(app)?)
                .build()?;
            let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/icon.ico"))
                .or_else(|_| {
                    app.default_window_icon()
                        .cloned()
                        .ok_or(tauri::Error::WindowNotFound)
                })?;
            let mute_for_menu = mute_item.clone();
            TrayIconBuilder::with_id("rev")
                .icon(icon)
                .tooltip("Rev — your gaming companion")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| {
                    let state: State<PanelState> = app.state();
                    match event.id().as_ref() {
                        "panel" => toggle_panel(app.clone()),
                        "mute" => {
                            let m = !state.muted.load(Ordering::Relaxed);
                            let _ = mute_for_menu.set_checked(m);
                            let _ = state.loop_tx.lock().unwrap().send(LoopMsg::SetMuted(m));
                        }
                        "autostart" => {
                            let on = !get_autostart();
                            let _ = set_autostart(on);
                        }
                        "quit" => {
                            let _ = state.loop_tx.lock().unwrap().send(LoopMsg::Shutdown);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_panel(tray.app_handle().clone());
                    }
                })
                .build(app)?;

            let deps = runloop::RunDeps {
                app: app.handle().clone(),
                loaded,
                capture: capture_shared.clone(),
                mic,
                tts_tx,
                loop_tx,
                rx: loop_rx,
                status,
                muted,
            };
            std::thread::Builder::new()
                .name("runloop".into())
                .spawn(move || runloop::run(deps))
                .expect("spawn runloop");
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "panel" {
                    api.prevent_close();
                    let _ = window.hide();
                } else if window.label() == "pet" {
                    // Closing the pet is quitting the app — but gracefully,
                    // through the run-loop's episode flush.
                    api.prevent_close();
                    let state: State<PanelState> = window.app_handle().state();
                    let _ = state.loop_tx.lock().unwrap().send(LoopMsg::Shutdown);
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
