//! Small shared helpers: the app-wide monotonic millisecond clock (every
//! `InputEvent` timestamp flows from here) and hotkey-name → VK mapping.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// Call once at startup so `now_ms()` is relative to app launch.
pub fn init_clock() {
    let _ = START.set(Instant::now());
}

/// Monotonic milliseconds since app start — the timebase for the
/// orchestrator's latency ledger.
pub fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Real local date for the prompt context block, e.g. "Monday, July 28 2026"
/// — without it models confidently answer date questions from their
/// training cutoff.
pub fn local_date_string() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    const DAYS: [&str; 7] =
        ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September",
        "October", "November", "December",
    ];
    let st = unsafe { GetLocalTime() };
    format!(
        "{}, {} {} {}",
        DAYS[(st.wDayOfWeek as usize) % 7],
        MONTHS[(st.wMonth as usize).clamp(1, 12) - 1],
        st.wDay,
        st.wYear
    )
}

/// Map a config hotkey name (config.toml `[hotkey] ptt`) to a Win32 virtual
/// key code. Names are case-insensitive.
pub fn vk_from_name(name: &str) -> Option<u32> {
    let n = name.trim().to_ascii_lowercase();
    // Function keys F1..F24 → 0x70..0x87.
    if let Some(num) = n.strip_prefix('f').and_then(|s| s.parse::<u32>().ok()) {
        if (1..=24).contains(&num) {
            return Some(0x70 + num - 1);
        }
    }
    // Single letters/digits map to their ASCII uppercase VK.
    if n.len() == 1 {
        let c = n.chars().next().unwrap().to_ascii_uppercase();
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    Some(match n.as_str() {
        "pageup" | "pgup" | "prior" => 0x21,
        "pagedown" | "pgdn" | "next" => 0x22,
        "end" => 0x23,
        "home" => 0x24,
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "pause" => 0x13,
        "scrolllock" | "scroll" => 0x91,
        "capslock" => 0x14,
        "backquote" | "grave" | "`" => 0xC0,
        "numpad0" => 0x60,
        "numpad1" => 0x61,
        "numpad2" => 0x62,
        "numpad3" => 0x63,
        "numpad4" => 0x64,
        "numpad5" => 0x65,
        "numpad6" => 0x66,
        "numpad7" => 0x67,
        "numpad8" => 0x68,
        "numpad9" => 0x69,
        "numpadmultiply" => 0x6A,
        "numpadadd" => 0x6B,
        "numpadsubtract" => 0x6D,
        "numpaddivide" => 0x6F,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_ptt_keys() {
        assert_eq!(vk_from_name("PageUp"), Some(0x21));
        assert_eq!(vk_from_name("pgup"), Some(0x21));
        assert_eq!(vk_from_name("F13"), Some(0x7C));
        assert_eq!(vk_from_name("scrolllock"), Some(0x91));
        assert_eq!(vk_from_name("q"), Some('Q' as u32));
        assert_eq!(vk_from_name("definitely-not-a-key"), None);
    }
}
