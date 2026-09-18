//! Voice chat on the host (`PUNKTFUNK_AUDIO_VOICE_CHAT=host`), the Windows half.
//!
//! Windows routes per application through the store `mmsys.cpl`'s "App volume and
//! device preferences" page writes: `AudioPolicyConfig::SetPersistedDefaultAudioEndpoint
//! (pid, flow, role, device)`, undocumented like the `IPolicyConfig` default-endpoint
//! write next door. The store is per user and the host is SYSTEM, so the write has to
//! come from the signed-in user's context: a worker thread finds voice-app processes
//! and runs this same binary as the console user, windowless
//! (`punktfunk-host voice-route set …`, [`cli`]), to pin them. `clear` sets each pin
//! back to "default". A host that is not SYSTEM writes in-process.
//!
//! A pin is keyed by the app, not the pid, and outlives both the process and a host
//! crash. The exe names pinned are kept in a marker file until each is cleared against
//! a running process — at session end, at the next host start, or the next session end
//! the app is running again ([`recover_orphaned`]).
//!
//! The target is the output the operator heard before the session parked the default
//! on the plan's sink ([`super::audio_control::parked_previous_render`]).

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

/// How often the worker looks for a voice app launched mid-session.
const RESCAN_EVERY: Duration = Duration::from_secs(5);
/// A pin helper that has not exited in this long is not going to.
const HELPER_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn wanted() -> bool {
    pf_host_config::config().audio_voice_chat == pf_host_config::VoiceChatRoute::Host
}

/// The capture thread's pins: which output, which pids point at it, and the worker
/// that writes them.
#[derive(Default)]
pub(crate) struct VoiceRoute {
    target: Option<String>,
    pinned: BTreeSet<u32>,
    last_scan: Option<Instant>,
    /// A worker's report: the pids and exe names it pinned.
    inflight: Option<Receiver<Vec<(u32, String)>>>,
}

impl VoiceRoute {
    /// Point voice apps at `device_id` for this capture. A changed target clears the old
    /// pins first; the next [`tick`](Self::tick) writes the new ones.
    pub(crate) fn arm(&mut self, device_id: &str) {
        if !wanted() || self.target.as_deref() == Some(device_id) {
            return;
        }
        self.clear();
        self.target = Some(device_id.to_owned());
        tracing::info!(
            device = device_id,
            "voice chat stays on the host output — pinning voice apps to it"
        );
    }

    /// Collect the last worker's pins and start the next scan when one is due. Nothing
    /// here touches a process or a snapshot: this runs on the capture thread.
    pub(crate) fn tick(&mut self) {
        if let Some(rx) = &self.inflight {
            match rx.try_recv() {
                Ok(pins) => {
                    self.inflight = None;
                    if !pins.is_empty() {
                        tracing::info!(pins = ?pins, "voice-chat apps pinned to the host output");
                        self.pinned.extend(pins.iter().map(|(pid, _)| *pid));
                        owe(pins.into_iter().map(|(_, exe)| exe));
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.inflight = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
            }
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        if self.last_scan.is_some_and(|t| t.elapsed() < RESCAN_EVERY) {
            return;
        }
        self.last_scan = Some(Instant::now());
        let known = self.pinned.clone();
        let (tx, rx) = channel();
        self.inflight = Some(rx);
        std::thread::Builder::new()
            .name("punktfunk-voice-pin".into())
            .spawn(move || {
                let apps = &pf_host_config::config().audio_voice_apps;
                let fresh: Vec<(u32, String)> = voice_processes(apps)
                    .into_iter()
                    .filter(|(pid, _)| !known.contains(pid))
                    .collect();
                let pids: Vec<u32> = fresh.iter().map(|(pid, _)| *pid).collect();
                let done = fresh.is_empty() || run_helper(&["set", &target, &csv(&pids)]);
                let _ = tx.send(if done { fresh } else { Vec::new() });
            })
            .map_err(|e| tracing::warn!(error = %e, "voice-chat pin worker did not start"))
            .ok();
    }

    /// Put every pinned app back on the default output: session end, or a target change.
    /// Blocking — the capture is stopping. An app that already exited stays owed.
    pub(crate) fn clear(&mut self) {
        self.target = None;
        self.last_scan = None;
        self.inflight = None;
        self.pinned.clear();
        recover_orphaned();
    }
}

/// Clear the pins the marker still owes, for every owed app that is running now. Host
/// start and session end; a crash or an app that exited before the clear leaves the
/// marker for the next chance.
pub(crate) fn recover_orphaned() {
    let owed = read_owed();
    if owed.is_empty() {
        return;
    }
    let apps: Vec<String> = owed.iter().cloned().collect();
    let running: Vec<(u32, String)> = voice_processes(&apps);
    let pids: Vec<u32> = running.iter().map(|(pid, _)| *pid).collect();
    if pids.is_empty() {
        tracing::debug!(owed = ?owed, "voice-chat pins owed to apps that are not running");
        return;
    }
    if !run_helper(&["clear", &csv(&pids)]) {
        return;
    }
    let cleared: BTreeSet<String> = running.into_iter().map(|(_, exe)| exe).collect();
    let left: BTreeSet<String> = owed.difference(&cleared).cloned().collect();
    write_owed(&left);
    tracing::info!(cleared = ?cleared, still_owed = ?left, "voice-chat pins cleared");
}

fn marker_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("voice-route.pinned")
}

/// Lowercase exe names whose pin has not been cleared yet, one per line.
fn read_owed() -> BTreeSet<String> {
    std::fs::read_to_string(marker_path())
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn write_owed(owed: &BTreeSet<String>) {
    let path = marker_path();
    if owed.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let body: String = owed.iter().map(|e| format!("{e}\n")).collect();
    if let Err(e) = std::fs::write(&path, body) {
        tracing::warn!(error = %e, "voice-chat pin marker not written — a crash would leave the pins");
    }
}

fn owe(exes: impl IntoIterator<Item = String>) {
    let mut owed = read_owed();
    owed.extend(exes);
    write_owed(&owed);
}

fn csv(pids: &[u32]) -> String {
    pids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// `(pid, lowercase exe name)` of every process carrying a voice-app fragment. Toolhelp,
/// as `procscan` does. Never on the capture thread.
fn voice_processes(apps: &[String]) -> Vec<(u32, String)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut out = Vec::new();
    // SAFETY: `entry` is zeroed with `dwSize` set before the first read; the snapshot handle
    // is closed on every exit path; `szExeFile` is read up to its first NUL.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..std::mem::zeroed()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let exe = String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase();
                if pf_host_config::voice_app_matches([exe.as_str()], apps) {
                    out.push((entry.th32ProcessID, exe));
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Run `voice-route <args>` as the console user, windowless, and wait for its verdict;
/// in-process when this host is not SYSTEM (a dev run already is the user).
fn run_helper(args: &[&str]) -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "voice-chat pin: own executable path unknown");
            return false;
        }
    };
    let cmdline = format!("\"{}\" voice-route {}", exe.display(), args.join(" "));
    match crate::windows::interactive::run_hidden_as_current_session_user(&cmdline, HELPER_TIMEOUT)
    {
        Ok(0) => true,
        Ok(code) => {
            tracing::warn!(
                code,
                "voice-chat pin helper refused — the apps stay where they are"
            );
            false
        }
        Err(spawn_err) => {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            match cli(&owned) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        spawn = %format!("{spawn_err:#}"),
                        "voice-chat pin not written — the apps stay where they are"
                    );
                    false
                }
            }
        }
    }
}

/// `punktfunk-host voice-route set <device-id> <pid,…>` / `clear <pid,…>`: the process
/// that writes the per-app pins, in whichever user context it was started in.
pub(crate) fn cli(args: &[String]) -> Result<()> {
    const USAGE: &str =
        "usage: punktfunk-host voice-route set <device-id> <pid,pid,…> | clear <pid,pid,…>";
    let arg = |i: usize| args.get(i).map(String::as_str).context(USAGE);
    let (device, pids) = match arg(0)? {
        "set" => (Some(arg(1)?), arg(2)?),
        "clear" => (None, arg(1)?),
        _ => bail!("{USAGE}"),
    };
    let pids: Vec<u32> = pids
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")?;
    let policy = AudioPolicyConfig::activate()?;
    let mut failed = 0usize;
    for pid in &pids {
        if let Err(e) = policy.set_persisted_render(*pid, device) {
            failed += 1;
            eprintln!("pid {pid}: {e:#}");
        }
    }
    if failed > 0 {
        bail!("{failed} of {} pins not written", pids.len());
    }
    Ok(())
}

/// `Windows.Media.Internal.AudioPolicyConfig`'s factory, by slot: IUnknown, IInspectable,
/// nineteen volume-group and chat methods this host never calls, then the three we do.
#[repr(C)]
struct IAudioPolicyConfigFactoryVtbl {
    query_interface: unsafe extern "system" fn(
        *mut c_void,
        *const windows::core::GUID,
        *mut *mut c_void,
    ) -> windows::core::HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    /// GetIids, GetRuntimeClassName, GetTrustLevel.
    _inspectable: [*const c_void; 3],
    _reserved: [*const c_void; 19],
    /// `(pid, EDataFlow, ERole, HSTRING device)`; a null HSTRING clears the pin.
    set_persisted_default_audio_endpoint: unsafe extern "system" fn(
        *mut c_void,
        u32,
        u32,
        u32,
        *mut c_void,
    ) -> windows::core::HRESULT,
    get_persisted_default_audio_endpoint: unsafe extern "system" fn(
        *mut c_void,
        u32,
        u32,
        u32,
        *mut *mut c_void,
    ) -> windows::core::HRESULT,
    clear_all_persisted_application_default_endpoints:
        unsafe extern "system" fn(*mut c_void) -> windows::core::HRESULT,
}

// No header exists; calls go by slot. A field added above `set_persisted…` would invoke a
// different method, so the slot indexes and the table size are pinned here.
const _: () = {
    use std::mem::{offset_of, size_of};
    type P = *const c_void;
    assert!(offset_of!(IAudioPolicyConfigFactoryVtbl, _inspectable) == 3 * size_of::<P>());
    assert!(offset_of!(IAudioPolicyConfigFactoryVtbl, _reserved) == 6 * size_of::<P>());
    assert!(
        offset_of!(
            IAudioPolicyConfigFactoryVtbl,
            set_persisted_default_audio_endpoint
        ) == 25 * size_of::<P>()
    );
    assert!(
        offset_of!(
            IAudioPolicyConfigFactoryVtbl,
            get_persisted_default_audio_endpoint
        ) == 26 * size_of::<P>()
    );
    assert!(
        offset_of!(
            IAudioPolicyConfigFactoryVtbl,
            clear_all_persisted_application_default_endpoints
        ) == 27 * size_of::<P>()
    );
    assert!(size_of::<IAudioPolicyConfigFactoryVtbl>() == 28 * size_of::<P>());
    // The HSTRING handle is passed by value: one pointer.
    assert!(size_of::<windows::core::HSTRING>() == size_of::<P>());
};

/// A live `IAudioPolicyConfigFactory`, released on drop.
struct AudioPolicyConfig {
    raw: *mut c_void,
}

impl AudioPolicyConfig {
    /// The IID changed in Windows 11 21H2; the newer one is tried first.
    fn activate() -> Result<AudioPolicyConfig> {
        use windows::core::{IInspectable, Interface, GUID, HSTRING};
        use windows::Win32::System::WinRT::RoGetActivationFactory;
        const IID_WIN11: GUID = GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);
        const IID_WIN10: GUID = GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258);
        let class = HSTRING::from("Windows.Media.Internal.AudioPolicyConfig");
        // SAFETY: `class` is a live HSTRING; the factory is an owned IInspectable released by
        // its Drop.
        let factory: IInspectable = unsafe { RoGetActivationFactory(&class) }
            .map_err(|e| anyhow!("RoGetActivationFactory(AudioPolicyConfig): {e}"))?;
        for iid in [IID_WIN11, IID_WIN10] {
            let mut raw: *mut c_void = std::ptr::null_mut();
            // SAFETY: QueryInterface on a live factory; a non-null result is an owned reference
            // this struct releases exactly once in Drop.
            if unsafe { factory.query(&iid, &mut raw) }.is_ok() && !raw.is_null() {
                return Ok(AudioPolicyConfig { raw });
            }
        }
        bail!("IAudioPolicyConfigFactory: neither the Windows 11 nor the Windows 10 interface answered")
    }

    fn vtbl(&self) -> &IAudioPolicyConfigFactoryVtbl {
        // SAFETY: `raw` is a live COM pointer whose first word is the vtable the asserts pin.
        unsafe { &**(self.raw as *const *const IAudioPolicyConfigFactoryVtbl) }
    }

    /// Pin `pid`'s render streams to `device_id`, or clear the pin with `None`. All three
    /// roles: a voice app may open its call audio on the communications role, and a pin
    /// that skipped it would leave exactly the voices in the stream.
    fn set_persisted_render(&self, pid: u32, device_id: Option<&str>) -> Result<()> {
        let hs = device_id.map(|id| windows::core::HSTRING::from(mmdevapi_path(id)));
        // SAFETY: HSTRING is one pointer (asserted above); the copy is the handle, which `hs`
        // keeps alive across the calls. Null = "default", as the Sound settings page writes.
        let handle: *mut c_void = hs.as_ref().map_or(std::ptr::null_mut(), |h| unsafe {
            std::mem::transmute_copy(h)
        });
        for role in 0u32..=2 {
            // SAFETY: live factory (`vtbl` above); eRender = 0; eConsole..eCommunications = 0..=2.
            let hr = unsafe {
                (self.vtbl().set_persisted_default_audio_endpoint)(self.raw, pid, 0, role, handle)
            };
            hr.ok().map_err(|e| {
                anyhow!("SetPersistedDefaultAudioEndpoint(pid {pid}, role {role}): {e}")
            })?;
        }
        Ok(())
    }
}

impl Drop for AudioPolicyConfig {
    fn drop(&mut self) {
        // SAFETY: `raw` is the owned reference `activate` took; released exactly once.
        unsafe { (self.vtbl().release)(self.raw) };
    }
}

/// The device-interface path the store keys on: the MMDevice id inside the
/// `SWD\MMDEVAPI` prefix and the render interface class.
fn mmdevapi_path(device_id: &str) -> String {
    format!("\\\\?\\SWD#MMDEVAPI#{device_id}#{{e6327cad-dcec-4949-ae8a-991e976a79d2}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_path_wraps_the_endpoint_id() {
        assert_eq!(
            mmdevapi_path("{0.0.0.00000000}.{aaaa}"),
            "\\\\?\\SWD#MMDEVAPI#{0.0.0.00000000}.{aaaa}#{e6327cad-dcec-4949-ae8a-991e976a79d2}"
        );
        assert_eq!(csv(&[7, 42]), "7,42");
    }
}
