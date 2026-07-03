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
//! * default **PLAYBACK** → a loopback-capable render that is NOT the mic target (a real output device
//!   if one exists, else the Steam Streaming Microphone; **never** the Steam Streaming Speakers, whose
//!   loopback is silent — validated live). This is the endpoint [`super::wasapi_cap`] captures;
//! * default **RECORDING** → the mic target's capture endpoint (VB-Cable "CABLE Output") so host apps
//!   record the client's mic by default.
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

/// Enumerate endpoints, compute the assignment, apply the default-device changes (unless
/// `PUNKTFUNK_KEEP_DEFAULT`), and return the plan for the caller to act on (mic target / loopback
/// echo guard). Must run on a COM-initialized thread (the WASAPI worker threads all
/// `initialize_mta` first). Logged only when the assignment changes, so per-open recomputation
/// stays quiet in the steady state.
pub(crate) fn wire_now() -> Wiring {
    let renders = list_endpoints(Direction::Render);
    let captures = list_endpoints(Direction::Capture);
    let want = std::env::var("PUNKTFUNK_MIC_DEVICE")
        .ok()
        .map(|s| s.to_lowercase());
    let wiring = plan(&renders, &captures, want.as_deref());

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
    if let Some((name, id)) = &wiring.loopback_render {
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
