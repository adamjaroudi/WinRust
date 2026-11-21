//! Per-app volume — what Windows calls the volume mixer: the audio sessions of the default playback
//! device, one row per program that is making sound. Enumerating them is a COM round trip per session,
//! so a worker thread reads the list and the flyout paints whatever it cached last.
use std::{
    ptr::null,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
};
use windows::{
    Win32::{
        Media::Audio::{AudioSessionStateExpired, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, eConsole, eRender},
        System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx},
    },
    core::Interface,
};

/// One program in the mixer. Several sessions of the same process are shown once and moved together.
#[derive(Clone, PartialEq)]
pub struct App {
    pub name: String,
    pub pid: u32,
    pub vol: i32,
    pub muted: bool,
}

static APPS: Mutex<Vec<App>> = Mutex::new(Vec::new());
static BUSY: AtomicBool = AtomicBool::new(false);

/// The apps as of the last refresh (the flyout calls this while painting, so it never blocks).
pub fn apps() -> Vec<App> {
    APPS.lock().map(|a| a.clone()).unwrap_or_default()
}

/// Reads the sessions again, one thread at a time.
pub fn refresh() {
    if BUSY.swap(true, Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        let v = unsafe { read() };
        if let Ok(mut a) = APPS.lock() {
            *a = v;
        }
        BUSY.store(false, Relaxed);
    });
}

/// Every live session of the default render device, with the volume interface for each.
unsafe fn sessions() -> Vec<(IAudioSessionControl2, ISimpleAudioVolume)> {
    // the worker thread runs COM calls: without CoInitializeEx they fail at once and the mixer stays empty
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let mut out = vec![];
    let _ = (|| -> windows::core::Result<()> {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let mgr: IAudioSessionManager2 = en.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None)?;
        let list = mgr.GetSessionEnumerator()?;
        for i in 0..list.GetCount()? {
            let Ok(c) = list.GetSession(i) else { continue };
            let (Ok(c2), Ok(v)) = (c.cast::<IAudioSessionControl2>(), c.cast::<ISimpleAudioVolume>()) else { continue };
            out.push((c2, v));
        }
        Ok(())
    })();
    out
}

unsafe fn read() -> Vec<App> {
    let mut v: Vec<App> = vec![];
    for (c, vol) in sessions() {
        // expired sessions are programs that have closed. The system-sounds session is not a program either, but
        // IsSystemSoundsSession answers S_OK and S_FALSE, which both arrive here as Ok and so cannot tell them
        // apart; its process id is 0, and the check below drops it with any other session we cannot name.
        if c.GetState().is_ok_and(|s| s == AudioSessionStateExpired) {
            continue;
        }
        let Ok(pid) = c.GetProcessId() else { continue };
        if pid == 0 || v.iter().any(|a| a.pid == pid) {
            continue; // one row per program, however many sessions it opened
        }
        let name = proc_name(pid).or_else(|| c.GetDisplayName().ok().and_then(|n| n.to_string().ok()).filter(|n| !n.is_empty()));
        let Some(name) = name else { continue };
        v.push(App {
            name,
            pid,
            vol: vol.GetMasterVolume().map(|f| (f * 100.0).round() as i32).unwrap_or(100),
            muted: vol.GetMute().is_ok_and(|m| m.as_bool()),
        });
    }
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    v
}

/// "C:\...\Spotify.exe" -> "Spotify": the mixer shows the program, not its path.
fn proc_name(pid: u32) -> Option<String> {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let (mut buf, mut n) = ([0u16; 260], 260u32);
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut n) != 0;
        windows_sys::Win32::Foundation::CloseHandle(h);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..n as usize]);
        let file = path.rsplit('\\').next()?.to_string();
        let stem = file.strip_suffix(".exe").unwrap_or(&file).to_string();
        let mut ch = stem.chars();
        Some(ch.next()?.to_uppercase().collect::<String>() + ch.as_str())
    }
}

/// Sets one program's volume (every session it owns), and unmutes it the way a master-volume drag does.
pub fn set_vol(pid: u32, v: i32) {
    bump(pid, |s| unsafe {
        let _ = s.SetMasterVolume(v.clamp(0, 100) as f32 / 100.0, null());
        let _ = s.SetMute(false, null());
    });
    if let Ok(mut a) = APPS.lock()
        && let Some(app) = a.iter_mut().find(|a| a.pid == pid)
    {
        (app.vol, app.muted) = (v.clamp(0, 100), false); // the slider follows the drag; refresh() confirms it
    }
}

/// Mutes or unmutes one program.
pub fn set_mute(pid: u32, m: bool) {
    bump(pid, |s| unsafe {
        let _ = s.SetMute(m, null());
    });
    if let Ok(mut a) = APPS.lock()
        && let Some(app) = a.iter_mut().find(|a| a.pid == pid)
    {
        app.muted = m;
    }
}

fn bump(pid: u32, f: impl Fn(&ISimpleAudioVolume)) {
    unsafe {
        for (c, v) in sessions() {
            if c.GetProcessId().is_ok_and(|p| p == pid) {
                f(&v);
            }
        }
    }
}
