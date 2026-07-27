//! Foreground game detection: poll the foreground window's exe path and run
//! it through the core detection ladder (Steam appmanifests → Discord
//! detectable DB). Emits `GameChanged` only on transitions.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use revolution_core::gamedetect::{
    exe_in_installdir, match_exe, parse_appmanifest, DetectableEntry, SteamApp,
};
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::msg::LoopMsg;

/// A Steam install known to this machine.
struct SteamInstall {
    app: SteamApp,
    /// `<library>\steamapps\common`
    common_dir: String,
}

fn quoted(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut inq = false;
    for c in line.chars() {
        match c {
            '"' => {
                if inq {
                    out.push(std::mem::take(&mut cur));
                }
                inq = !inq;
            }
            _ if inq => cur.push(c),
            _ => {}
        }
    }
    out
}

/// Steam root from the registry (HKCU\Software\Valve\Steam → SteamPath).
fn steam_root() -> Option<PathBuf> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let key = hkcu.open_subkey("Software\\Valve\\Steam").ok()?;
    let path: String = key.get_value("SteamPath").ok()?;
    Some(PathBuf::from(path))
}

/// Scan every Steam library's appmanifests once at startup.
fn scan_steam() -> Vec<SteamInstall> {
    let mut out = Vec::new();
    let Some(root) = steam_root() else { return out };
    let mut libraries = vec![root.clone()];
    if let Ok(vdf) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
        for line in vdf.lines() {
            let q = quoted(line);
            if q.len() >= 2 && q[0].eq_ignore_ascii_case("path") {
                libraries.push(PathBuf::from(q[1].replace("\\\\", "\\")));
            }
        }
    }
    libraries.dedup();
    for lib in libraries {
        let steamapps = lib.join("steamapps");
        let Ok(entries) = std::fs::read_dir(&steamapps) else { continue };
        let common = steamapps.join("common").to_string_lossy().to_string();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("appmanifest_") && name.ends_with(".acf") {
                if let Ok(text) = std::fs::read_to_string(entry.path()) {
                    if let Some(app) = parse_appmanifest(&text) {
                        out.push(SteamInstall { app, common_dir: common.clone() });
                    }
                }
            }
        }
    }
    out
}

/// Optional cached Discord detectable DB (`%APPDATA%\revolution\detectable.json`).
fn load_detectable(dir: &Path) -> Vec<DetectableEntry> {
    std::fs::read_to_string(dir.join("detectable.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn foreground_exe() -> Option<(u32, String)> {
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        ok.ok()?;
        Some((pid, String::from_utf16_lossy(&buf[..len as usize])))
    }
}

/// Detection ladder: Steam install dirs, then the detectable DB.
fn detect(steam: &[SteamInstall], db: &[DetectableEntry], exe: &str) -> Option<String> {
    for s in steam {
        if exe_in_installdir(exe, &s.common_dir, &s.app.installdir) {
            return Some(s.app.name.clone());
        }
    }
    match_exe(db, exe).map(|e| e.name.clone())
}

pub fn spawn(tx: Sender<LoopMsg>, config_dir: PathBuf) {
    std::thread::Builder::new()
        .name("gamewatch".into())
        .spawn(move || {
            let steam = scan_steam();
            let db = load_detectable(&config_dir);
            let me = std::process::id();
            let mut last_exe = String::new();
            loop {
                if let Some((pid, exe)) = foreground_exe() {
                    // Our own windows keep the previous game state.
                    if pid != me && exe != last_exe {
                        last_exe = exe.clone();
                        let game = detect(&steam, &db, &exe).map(|name| (name, exe.clone()));
                        let _ = tx.send(LoopMsg::GameChanged { exe_path: exe, game });
                    }
                }
                std::thread::sleep(Duration::from_millis(1500));
            }
        })
        .expect("spawn gamewatch thread");
}
