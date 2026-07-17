//! Windows audio device auto-wiring — production mic + desktop-audio passthrough with zero manual
//! setup.
//!
//! A headless host has no real audio output, so BOTH the desktop-audio loopback ([`super::wasapi_cap`])
//! and the virtual mic ([`super::wasapi_mic`]) must run on VIRTUAL audio cables — and on DIFFERENT
//! ones, or the loopback re-captures the injected mic (an infinite echo). The installer bundles
//! VB-Audio Virtual Cable (the mic target: its "CABLE Input" render endpoint → "CABLE Output" capture)
//! and the host auto-installs the Steam Streaming pair (a loopback-capable render). This module wires
//! them up so no manual Sound-settings fiddling is ever needed:
//!
//! * the **mic inject target** is assigned FIRST (VB-Cable "CABLE Input" preferred) — mic passthrough
//!   is what the cable is bundled for, so it wins the cable even when the cable is the only render
//!   endpoint on the box (the loopback then reports itself unavailable instead of echoing);
//! * default **PLAYBACK** → the plan's loopback endpoint, applied ONLY while a desktop-audio capture
//!   is open (`set_playback` — the mic pump must never park the playback default while the host is
//!   idle). By default that endpoint is the SILENT sink (Steam Streaming Microphone render side) so
//!   audio plays on the client only; `PUNKTFUNK_HOST_AUDIO` prefers real hardware instead (audible on
//!   both ends). **Never** the Steam Streaming Speakers, whose loopback is silent — validated live;
//! * default **RECORDING** → the mic target's capture endpoint (VB-Cable "CABLE Output") so host apps
//!   record the client's mic by default.
//!
//! Because the playback default is *parked* on a silent sink during a stream, it is remembered
//! ([`park_default_playback`], plus an on-disk crash marker) and put back when the capture closes
//! ([`restore_default_playback`]) or, after a crash, on the next process's first wiring pass — an
//! operator must never be stranded with silent speakers. A default the operator changed themselves
//! mid-stream is respected (no restore over their choice).
//!
//! The assignment rules are the PURE [`wiring_plan`](super::wiring_plan) module (unit-tested on every
//! platform); this module only enumerates endpoints, applies the plan, and logs. [`wire_now`] runs on
//! every mic/capture (re)open — NOT once per process — because endpoints churn (boot-time
//! registration, hotplug, driver installs) and a stale plan was one of the ways mic passthrough died
//! permanently.
//!
//! Setting a default endpoint uses the undocumented `IPolicyConfig` COM interface (the only way to set
//! a default device programmatically — neither the `windows` nor `wasapi` crate exposes it; it is the
//! same call `mmsys.cpl` makes). Opt out with `PUNKTFUNK_KEEP_DEFAULT` to leave the user's chosen
//! defaults untouched (the plan is still computed — the mic must still pick a target).

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::wiring_plan::{plan, Endpoint, Wiring};
use anyhow::{anyhow, bail, Result};
use std::ffi::c_void;
use std::sync::Mutex;
use wasapi::Direction;

/// `(friendly_name, endpoint_id)` for every ACTIVE endpoint in direction `dir`.
fn list_endpoints(dir: Direction) -> Vec<Endpoint> {
    let mut out = Vec::new();
    let Ok(en) = wasapi::DeviceEnumerator::new() else {
        return out;
    };
    let Ok(coll) = en.get_device_collection(&dir) else {
        return out;
    };
    let Ok(n) = coll.get_nbr_devices() else {
        return out;
    };
    for i in 0..n {
        if let Ok(dev) = coll.get_device_at_index(i) {
            let id = dev.get_id().unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            out.push((dev.get_friendlyname().unwrap_or_default(), id));
        }
    }
    out
}

/// `PUNKTFUNK_HOST_AUDIO`: the operator wants the stream audible on the host too — the loopback
/// plan prefers real hardware over the silent sink (the pre-client-only-default behavior).
pub(crate) fn host_audio_requested() -> bool {
    std::env::var_os("PUNKTFUNK_HOST_AUDIO").is_some()
}

/// Enumerate endpoints, compute the assignment, apply the default-device changes (unless
/// `PUNKTFUNK_KEEP_DEFAULT`), and return the plan for the caller to act on (mic target / loopback
/// echo guard). `set_playback` — true only from the desktop-audio capture open — additionally
/// parks the default PLAYBACK device on the plan's loopback endpoint for the capture's lifetime
/// (the mic pump passes false: it runs while the host is idle and must not silence the box).
/// Must run on a COM-initialized thread (the WASAPI worker threads all `initialize_mta` first).
/// Logged only when the assignment changes, so per-open recomputation stays quiet in the steady
/// state.
pub(crate) fn wire_now(set_playback: bool) -> Wiring {
    recover_orphaned_default();
    let renders = list_endpoints(Direction::Render);
    let captures = list_endpoints(Direction::Capture);
    let want = std::env::var("PUNKTFUNK_MIC_DEVICE")
        .ok()
        .map(|s| s.to_lowercase());
    let wiring = plan(&renders, &captures, want.as_deref(), host_audio_requested());

    // Log assignment changes exactly once (first plan included).
    static LAST: Mutex<Option<Wiring>> = Mutex::new(None);
    let changed = {
        let mut last = LAST.lock().unwrap();
        let changed = last.as_ref() != Some(&wiring);
        *last = Some(wiring.clone());
        changed
    };
    if changed {
        tracing::info!(
            mic_render = wiring.mic_render.as_ref().map(|(n, _)| n.as_str()),
            mic_capture = wiring.mic_capture.as_ref().map(|(n, _)| n.as_str()),
            loopback_render = wiring.loopback_render.as_ref().map(|(n, _)| n.as_str()),
            renders = ?renders.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            "audio wiring plan"
        );
        if wiring.mic_render.is_some() && wiring.loopback_render.is_none() {
            tracing::warn!(
                "the virtual mic reserved the only usable render endpoint — desktop audio will be \
                 unavailable until another output device exists (attach one, or let the host \
                 install the Steam Streaming pair)"
            );
        }
    }

    if std::env::var_os("PUNKTFUNK_KEEP_DEFAULT").is_some() {
        if changed {
            tracing::info!(
                "PUNKTFUNK_KEEP_DEFAULT set — leaving the audio default devices untouched"
            );
        }
        return wiring;
    }
    // Default-playback hygiene, on EVERY wire (mic pump at boot included): if the default render
    // endpoint IS the mic target — VB-CABLE installs have been seen grabbing the default — every
    // app renders its audio INTO the virtual mic: recorders hear the desktop mix and the operator
    // hears nothing. Move the default to an audible endpoint. The pre-split wire_now covered this
    // as a side effect of always setting the playback default; the `set_playback` split must not
    // lose it — and it must run BEFORE parking, so a parked `prev` can never be the mic target
    // (restoring the cable as default after a stream would re-break the box).
    if let Some((mic_name, mic_id)) = &wiring.mic_render {
        if default_render_id().as_deref() == Some(mic_id.as_str()) {
            // Audible preference = the host_audio plan's loopback pick (real hardware first).
            match plan(&renders, &captures, want.as_deref(), true).loopback_render {
                Some((name, id)) => match set_default_endpoint(&id) {
                    Ok(()) => tracing::info!(mic = %mic_name, device = %name,
                        "default playback was the virtual-mic target — moved it so desktop \
                         audio no longer feeds the mic"),
                    Err(e) => tracing::warn!(device = %name, error = %format!("{e:#}"),
                        "failed to move the default playback off the virtual-mic target"),
                },
                None => {
                    if changed {
                        tracing::warn!(mic = %mic_name,
                            "default playback is the virtual-mic target and no other usable \
                             render endpoint exists — desktop audio will feed the mic");
                    }
                }
            }
        }
    }
    if set_playback {
        if let Some((name, id)) = &wiring.loopback_render {
            let mic_id = wiring.mic_render.as_ref().map(|(_, m)| m.as_str());
            park_default_playback(name, id, changed, mic_id);
        }
    }
    if let Some((name, id)) = &wiring.mic_capture {
        match set_default_endpoint(id) {
            Ok(()) => {
                if changed {
                    tracing::info!(device = %name,
                        "audio wiring: default recording = virtual mic (apps record the client's mic)");
                }
            }
            Err(e) => tracing::warn!(device = %name, error = %format!("{e:#}"),
                "audio wiring: failed to set the default recording device"),
        }
    }
    wiring
}

/// The operator's default playback endpoint while we have it parked on the loopback sink:
/// `(previous_id, id_we_set)`. In-memory source of truth; mirrored to [`park_marker_path`] so a
/// crashed host can't strand the box on the silent sink.
static PARKED: Mutex<Option<(String, String)>> = Mutex::new(None);

/// On-disk crash marker mirroring [`PARKED`] (two lines: previous id, set id).
fn park_marker_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("audio-default.prev")
}

/// The current default RENDER endpoint id, if any.
fn default_render_id() -> Option<String> {
    wasapi::DeviceEnumerator::new()
        .ok()?
        .get_default_device(&Direction::Render)
        .ok()?
        .get_id()
        .ok()
}

/// Once per process: if a crash marker from a previous run exists, the host died while the
/// playback default was parked — put the operator's device back, but only if the default still
/// IS the endpoint we set (a manual change since the crash wins). Runs on the first wiring pass
/// (the mic pump wires eagerly at host start, so this fires at boot, not at the first stream).
fn recover_orphaned_default() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let path = park_marker_path();
        let Ok(s) = std::fs::read_to_string(&path) else {
            return;
        };
        let _ = std::fs::remove_file(&path);
        let mut lines = s.lines();
        let (Some(prev), Some(set)) = (lines.next(), lines.next()) else {
            return;
        };
        if default_render_id().as_deref() != Some(set) {
            return;
        }
        match set_default_endpoint(prev) {
            Ok(()) => tracing::info!(
                "restored the default playback device a previous host run left parked"
            ),
            Err(e) => tracing::warn!(error = %format!("{e:#}"),
                "failed to restore the default playback device left by a previous run"),
        }
    });
}

/// Make `id` the default playback device for the duration of the desktop-audio capture,
/// remembering the operator's current default (in memory + the crash marker) the FIRST time so
/// [`restore_default_playback`] can put it back. Nothing is remembered when `id` already is the
/// default — there is nothing to restore. The MIC target is never remembered as the previous
/// default (restoring it would feed desktop audio into the virtual mic — the hygiene pass in
/// [`wire_now`] normally moved the default off it already; this guards the propagation race).
fn park_default_playback(name: &str, id: &str, changed: bool, mic_id: Option<&str>) {
    let cur = default_render_id();
    if cur.as_deref() != Some(id) {
        let mut parked = PARKED.lock().unwrap();
        match parked.as_mut() {
            None => {
                if let Some(prev) = cur.filter(|c| Some(c.as_str()) != mic_id) {
                    let _ = std::fs::write(park_marker_path(), format!("{prev}\n{id}"));
                    *parked = Some((prev, id.to_string()));
                }
            }
            // Re-park onto a different endpoint mid-stream (plan changed): keep the ORIGINAL
            // previous default, update what we set.
            Some((prev, set)) if set != id => {
                let _ = std::fs::write(park_marker_path(), format!("{prev}\n{id}"));
                *set = id.to_string();
            }
            Some(_) => {}
        }
    }
    match set_default_endpoint(id) {
        Ok(()) => {
            if changed {
                tracing::info!(device = %name,
                    "audio wiring: default playback = desktop-audio loopback source");
            }
        }
        Err(e) => tracing::warn!(device = %name, error = %format!("{e:#}"),
            "audio wiring: failed to set the default playback device"),
    }
}

/// Put the operator's default playback device back after streaming — the inverse of
/// [`park_default_playback`]. No-op if we never parked it, and a default the operator changed
/// themselves mid-stream is left alone (their choice wins). Must run on a COM-initialized thread
/// (called from the capture thread's exit path).
pub(crate) fn restore_default_playback() {
    let Some((prev, set)) = PARKED.lock().unwrap().take() else {
        return;
    };
    let _ = std::fs::remove_file(park_marker_path());
    if default_render_id().as_deref() != Some(set.as_str()) {
        return;
    }
    match set_default_endpoint(&prev) {
        Ok(()) => tracing::info!("default playback device restored after streaming"),
        Err(e) => tracing::warn!(error = %format!("{e:#}"),
            "failed to restore the default playback device after streaming"),
    }
}

/// Open a device by endpoint id, with a name for error context.
pub(crate) fn open_endpoint(ep: &Endpoint) -> Result<wasapi::Device> {
    wasapi::DeviceEnumerator::new()
        .map_err(|e| anyhow!("DeviceEnumerator: {e}"))?
        .get_device(&ep.1)
        .map_err(|e| anyhow!("open endpoint {:?}: {e}", ep.0))
}

// --- IPolicyConfig (undocumented): set a default audio endpoint by id, for all three roles. ---

/// The `IPolicyConfig` vtable. Only `SetDefaultEndpoint` is called; the 10 methods between `Release`
/// and it (`GetMixFormat` … `SetPropertyValue`) are placeholders so the slot offset is correct.
#[repr(C)]
struct IPolicyConfigVtbl {
    query_interface: unsafe extern "system" fn(
        *mut c_void,
        *const windows::core::GUID,
        *mut *mut c_void,
    ) -> windows::core::HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    _reserved: [*const c_void; 10],
    set_default_endpoint: unsafe extern "system" fn(
        *mut c_void,
        windows::core::PCWSTR,
        u32,
    ) -> windows::core::HRESULT,
    // SetEndpointVisibility follows — unused.
}

/// Set `device_id` as the default audio endpoint for eConsole/eMultimedia/eCommunications via the
/// undocumented `IPolicyConfig::SetDefaultEndpoint` (the call `mmsys.cpl` makes). Errs if any role
/// fails.
fn set_default_endpoint(device_id: &str) -> Result<()> {
    use windows::core::{IUnknown, Interface, GUID, PCWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    // PolicyConfigClient coclass + IPolicyConfig (Win7+) IID.
    const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
    const IID_IPOLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: CoCreateInstance with a valid CLSID returns an owned, refcounted IUnknown. We QI it for
    // IPolicyConfig; on success (HRESULT ok + non-null pointer) we invoke its SetDefaultEndpoint slot
    // through the documented vtable layout (3 IUnknown + 10 placeholder methods precede it) with a
    // NUL-terminated UTF-16 id and an in-range ERole (0..=2), then Release the QI'd pointer. Every
    // pointer is checked non-null before deref; `unk` is Released by its Drop on scope exit.
    unsafe {
        let unk: IUnknown = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)
            .map_err(|e| anyhow!("CoCreateInstance(PolicyConfig): {e}"))?;
        let mut raw: *mut c_void = std::ptr::null_mut();
        unk.query(&IID_IPOLICY_CONFIG, &mut raw)
            .ok()
            .map_err(|e| anyhow!("QueryInterface(IPolicyConfig): {e}"))?;
        if raw.is_null() {
            bail!("IPolicyConfig QueryInterface returned null");
        }
        let vtbl = *(raw as *const *const IPolicyConfigVtbl);
        let mut result = Ok(());
        for role in 0u32..=2 {
            let hr = ((*vtbl).set_default_endpoint)(raw, PCWSTR(wide.as_ptr()), role);
            if hr.is_err() {
                result = hr
                    .ok()
                    .map_err(|e| anyhow!("SetDefaultEndpoint(role {role}): {e}"));
            }
        }
        ((*vtbl).release)(raw);
        result
    }
}
