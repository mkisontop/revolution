//! Push-to-talk hotkey via a low-level keyboard hook (`WH_KEYBOARD_LL`).
//!
//! A dedicated thread owns the hook and its message pump. Auto-repeat
//! KEYDOWNs are filtered here; hold-vs-toggle semantics live in the
//! run-loop. The key is NOT swallowed — the game still sees it (v0 choice,
//! least surprising; make it configurable later).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use revolution_core::orchestrator::InputEvent;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::msg::LoopMsg;
use crate::util::now_ms;

static TX: OnceLock<Sender<LoopMsg>> = OnceLock::new();
static PTT_VK: AtomicU32 = AtomicU32::new(0x21); // PageUp default
static HELD: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        if kb.vkCode == PTT_VK.load(Ordering::Relaxed) {
            let msg = wparam.0 as u32;
            let ev = match msg {
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    // Filter key auto-repeat: only the first down counts.
                    (!HELD.swap(true, Ordering::Relaxed))
                        .then(|| InputEvent::PttDown { ms: now_ms() })
                }
                WM_KEYUP | WM_SYSKEYUP => {
                    HELD.store(false, Ordering::Relaxed);
                    Some(InputEvent::PttUp { ms: now_ms() })
                }
                _ => None,
            };
            if let (Some(ev), Some(tx)) = (ev, TX.get()) {
                let _ = tx.send(LoopMsg::Input(ev));
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Install the hook on its own thread. `key_name` comes from config
/// (`[hotkey] ptt`); unknown names fall back to PageUp with a warning.
pub fn spawn(key_name: &str, tx: Sender<LoopMsg>) -> Option<String> {
    let mut warning = None;
    let vk = match crate::util::vk_from_name(key_name) {
        Some(vk) => vk,
        None => {
            warning = Some(format!("unknown hotkey '{key_name}' — using PageUp"));
            0x21
        }
    };
    PTT_VK.store(vk, Ordering::Relaxed);
    let _ = TX.set(tx);

    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(|| unsafe {
            if SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0).is_err() {
                return; // hook refused (another security tool?) — typed input still works
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        })
        .expect("spawn hotkey thread");
    warning
}
