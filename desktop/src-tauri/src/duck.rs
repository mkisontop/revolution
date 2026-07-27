//! Game-audio ducking: on `Action::DuckGameAudio(true)` every render
//! session that isn't ours is dropped to 25% of its current volume; on
//! `false` the saved volumes are restored. Sessions are re-enumerated each
//! time (never cache COM interfaces across calls).
//!
//! The calling thread must have COM initialized (the run-loop does MTA init).

use std::collections::HashMap;

use windows::core::Interface;
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

const DUCK_FACTOR: f32 = 0.25;

/// Count other processes' render sessions (smoke-test plumbing check —
/// touches nothing). Caller must have COM initialized.
pub fn probe_session_count() -> windows::core::Result<u32> {
    let mut n = 0;
    Ducker::for_each_session(|_pid, _vol| {
        n += 1;
        Ok(())
    })?;
    Ok(n)
}

#[derive(Default)]
pub struct Ducker {
    /// pid → volume before ducking.
    saved: HashMap<u32, f32>,
}

impl Ducker {
    pub fn set(&mut self, duck: bool) {
        if let Err(e) = self.try_set(duck) {
            eprintln!("duck({duck}) failed: {e}");
        }
    }

    fn for_each_session(
        mut f: impl FnMut(u32, &ISimpleAudioVolume) -> windows::core::Result<()>,
    ) -> windows::core::Result<()> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
            let mgr: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
            let sessions = mgr.GetSessionEnumerator()?;
            let me = std::process::id();
            for i in 0..sessions.GetCount()? {
                let Ok(ctl) = sessions.GetSession(i) else { continue };
                let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
                let Ok(pid) = ctl2.GetProcessId() else { continue };
                if pid == me || pid == 0 {
                    continue;
                }
                let Ok(vol) = ctl.cast::<ISimpleAudioVolume>() else { continue };
                let _ = f(pid, &vol);
            }
        }
        Ok(())
    }

    fn try_set(&mut self, duck: bool) -> windows::core::Result<()> {
        if duck {
            if !self.saved.is_empty() {
                return Ok(()); // already ducked
            }
            let mut saved = HashMap::new();
            Self::for_each_session(|pid, vol| unsafe {
                let cur = vol.GetMasterVolume()?;
                saved.entry(pid).or_insert(cur);
                vol.SetMasterVolume(cur * DUCK_FACTOR, std::ptr::null())?;
                Ok(())
            })?;
            self.saved = saved;
        } else {
            if self.saved.is_empty() {
                return Ok(());
            }
            let saved = std::mem::take(&mut self.saved);
            Self::for_each_session(|pid, vol| unsafe {
                if let Some(&old) = saved.get(&pid) {
                    vol.SetMasterVolume(old, std::ptr::null())?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}
