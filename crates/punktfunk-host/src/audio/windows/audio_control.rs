//! Windows audio device auto-wiring — production mic + desktop-audio passthrough with zero manual
//! setup.
//!
//! A headless host has no real audio output, so BOTH the desktop-audio loopback ([`super::wasapi_cap`])
//! and the virtual mic ([`super::wasapi_mic`]) must run on VIRTUAL audio cables — and on DIFFERENT
//! ones, or the loopback re-captures the injected mic (an infinite echo). The installer bundles
//! VB-Audio Virtual Cable (the mic target: its "CABLE Input" render endpoint → "CABLE Output" capture)
//! and the host auto-installs the Steam Streaming pair (a loopback-capable render). This module wires
//! them up at startup so no manual Sound-settings fiddling is ever needed:
//!
//! * default **PLAYBACK**  → a loopback-capable render that is NOT the mic cable (a real output device
//!   if one exists, else the Steam Streaming Microphone; **never** the Steam Streaming Speakers, whose
//!   loopback is silent — validated live). This is the endpoint [`super::wasapi_cap`] loopback-captures
//!   for desktop audio.
//! * default **RECORDING** → the virtual mic's capture endpoint (VB-Cable "CABLE Output") so host apps
//!   record the client's mic by default.
//!
//! [`super::wasapi_mic::find_device`] then resolves the mic INJECT target to "CABLE Input" — a render
//! candidate that is NOT the default playback — guaranteeing loopback ≠ mic, so there is no echo.
//!
//! Setting a default endpoint uses the undocumented `IPolicyConfig` COM interface (the only way to set
//! a default device programmatically — neither the `windows` nor `wasapi` crate exposes it; it is the
//! same call `mmsys.cpl` makes). Opt out with `PUNKTFUNK_KEEP_DEFAULT` to leave the user's chosen
//! defaults untouched.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::{anyhow, bail, Result};
use std::ffi::c_void;
use std::sync::Once;
use wasapi::Direction;

/// Run the audio device auto-wiring exactly once per process, before the first capturer/mic opens.
/// Blocks until done so the default playback is set before the loopback captures it. Best-effort:
/// every failure is logged, never fatal (the host then falls back to whatever the current defaults
/// are — exactly the pre-wiring behaviour).
pub(crate) fn ensure_wired_once() {
    static WIRED: Once = Once::new();
    WIRED.call_once(|| {
        if std::env::var_os("PUNKTFUNK_KEEP_DEFAULT").is_some() {
            tracing::info!("PUNKTFUNK_KEEP_DEFAULT set — leaving the audio default devices untouched");
            return;
        }
        // Run on a dedicated COM-MTA thread so we never collide with the caller's apartment mode
        // (the capture/mic threads each initialize their own COM separately).
        let handle = std::thread::Builder::new()
            .name("pf-audio-wiring".into())
            .spawn(|| {
                if wasapi::initialize_mta().ok().is_err() {
                    tracing::warn!("audio wiring: COM init (MTA) failed — skipping");
                    return;
                }
                if let Err(e) = ensure_audio_wiring() {
                    tracing::warn!(error = %format!("{e:#}"),
                        "audio auto-wiring failed — mic/desktop audio may need manual device defaults");
                }
            });
        if let Ok(h) = handle {
            let _ = h.join();
        }
    });
}

/// `(friendly_name, endpoint_id)` for every ACTIVE endpoint in direction `dir`.
fn list_endpoints(dir: Direction) -> Vec<(String, String)> {
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

/// Pick the loopback + mic-capture devices and set them as the default playback/recording.
fn ensure_audio_wiring() -> Result<()> {
    let renders = list_endpoints(Direction::Render);
    let captures = list_endpoints(Direction::Capture);
    if renders.is_empty() {
        bail!("no active render endpoints to wire");
    }

    // A render is unusable as the desktop-audio loopback if it is a VB-Cable endpoint (reserved for
    // the mic inject) or the Steam Streaming Speakers (its loopback is silent — validated live).
    let excluded_loopback =
        |ln: &str| ln.contains("cable") || ln.contains("steam streaming speakers");
    // "virtual-ish" = a known virtual cable; a render WITHOUT these markers is a real output device,
    // the best loopback source (apps render there and the operator can also hear it).
    let virtualish = |ln: &str| {
        ln.contains("virtual")
            || ln.contains("cable")
            || ln.contains("steam streaming")
            || ln.contains("voicemeeter")
    };
    let loopback = renders
        .iter()
        .find(|(n, _)| {
            let ln = n.to_lowercase();
            !excluded_loopback(&ln) && !virtualish(&ln)
        })
        .or_else(|| {
            renders
                .iter()
                .find(|(n, _)| n.to_lowercase().contains("steam streaming microphone"))
        })
        .or_else(|| {
            renders
                .iter()
                .find(|(n, _)| !excluded_loopback(&n.to_lowercase()))
        });

    // The virtual mic's CAPTURE endpoint host apps record from — VB-Cable "CABLE Output" preferred.
    let mic_capture = captures
        .iter()
        .find(|(n, _)| n.to_lowercase().contains("cable output"))
        .or_else(|| {
            captures
                .iter()
                .find(|(n, _)| n.to_lowercase().contains("steam streaming microphone"))
        })
        .or_else(|| {
            captures.iter().find(|(n, _)| {
                let ln = n.to_lowercase();
                ln.contains("voicemeeter") || ln.contains("virtual")
            })
        });

    match loopback {
        Some((name, id)) => match set_default_endpoint(id) {
            Ok(()) => tracing::info!(device = %name,
                "audio wiring: default playback = desktop-audio loopback source"),
            Err(e) => tracing::warn!(device = %name, error = %format!("{e:#}"),
                "audio wiring: failed to set the default playback device"),
        },
        None => {
            tracing::warn!("audio wiring: no usable desktop-audio loopback render endpoint found")
        }
    }
    if let Some((name, id)) = mic_capture {
        match set_default_endpoint(id) {
            Ok(()) => tracing::info!(device = %name,
                "audio wiring: default recording = virtual mic (apps record the client's mic)"),
            Err(e) => tracing::warn!(device = %name, error = %format!("{e:#}"),
                "audio wiring: failed to set the default recording device"),
        }
    }
    Ok(())
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
