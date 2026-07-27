//! Revolution desktop shell entrypoint: `--smoke` for headless hardware
//! validation, otherwise the Tauri pet + panel with every service thread.

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

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use tauri::{Manager, State};

use crate::capture::CaptureShared;
use crate::msg::LoopMsg;
use crate::runloop::Status;

struct PanelState {
    loop_tx: Mutex<Sender<LoopMsg>>,
    status: Arc<Mutex<Status>>,
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

    let mut initial_status = Status {
        config_path: loaded.path.to_string_lossy().to_string(),
        warnings: loaded.warnings.clone(),
        ..Status::default()
    };
    if let Some(w) = hotkey::spawn(&loaded.cfg.hotkey.ptt, loop_tx.clone()) {
        initial_status.warnings.push(w);
    }
    let status = Arc::new(Mutex::new(initial_status));

    let mic = mic::spawn();
    let tts_tx = tts::spawn(loaded.cfg.voice.tts.clone(), loop_tx.clone());
    capture::start(capture_shared.clone());
    gamewatch::spawn(loop_tx.clone(), config_io::config_dir());

    let panel_state = PanelState {
        loop_tx: Mutex::new(loop_tx.clone()),
        status: status.clone(),
    };

    tauri::Builder::default()
        .manage(panel_state)
        .invoke_handler(tauri::generate_handler![
            send_typed,
            remember_note,
            get_status,
            get_config_text,
            save_config_text,
            save_secret,
            open_config_dir,
            toggle_panel
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

            let deps = runloop::RunDeps {
                app: app.handle().clone(),
                loaded,
                capture: capture_shared.clone(),
                mic,
                tts_tx,
                loop_tx,
                rx: loop_rx,
                status,
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
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
