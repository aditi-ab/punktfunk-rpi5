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
//!   audio plays on the client only; `audio.output_mode = host_and_client` (formerly
//!   `PUNKTFUNK_HOST_AUDIO`) prefers real hardware instead (audible on both ends). Since 2026-08 a
//!   silent sink must also be able to CARRY the mix — one that narrows it (a voice-carrier endpoint
//!   mixing mono or at 24 kHz) loses to real hardware; see [`super::wiring_plan`]. **Never** the
//!   Steam Streaming Speakers, whose loopback is silent — validated live;
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
//! same call `mmsys.cpl` makes). The `audio.output_mode = follow_default` setting (formerly
//! `PUNKTFUNK_KEEP_DEFAULT`) leaves the user's chosen defaults untouched — the plan is still
//! computed, since the mic must still pick a target.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::wiring_plan::{self, plan, plan_with_formats, Endpoint, MixFormat, Wiring};
use anyhow::{anyhow, bail, Result};
use std::ffi::c_void;
use std::sync::Mutex;
use wasapi::Direction;

/// A render endpoint's engine mix format, or `None` if it cannot be asked right now.
///
/// This is the number the 2026-08-03 field report needed and no log had: the capture side requests
/// 48 kHz f32 with `autoconvert`, so WASAPI converts silently from whatever the endpoint really
/// runs — and a voice-carrier endpoint (Steam's Streaming Microphone) narrowing the desktop mix to
/// mono or 24 kHz was invisible. Reading it costs one `IAudioClient` activation per endpoint, done
/// only during a wiring pass.
///
/// Deliberately total: EVERY failure maps to `None` ("assume it is fine"), because the wiring plan
/// treats an unknown format as non-narrowing. A box where activation fails therefore plans exactly
/// as it did before formats existed, instead of mis-demoting a perfectly good endpoint.
fn mix_format_of(ep: &Endpoint) -> Option<MixFormat> {
    let fmt = open_endpoint(ep)
        .ok()?
        .get_iaudioclient()
        .ok()?
        .get_mixformat()
        .ok()?;
    Some(MixFormat {
        rate_hz: fmt.get_samplespersec(),
        channels: fmt.get_nchannels(),
        bits: fmt.get_bitspersample(),
    })
}

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

/// The operator wants the stream audible on the host too — the loopback plan prefers real
/// hardware over the silent sink (the pre-client-only-default behavior).
///
/// Now driven by the first-class `audio.output_mode` setting
/// ([`AudioOutputMode`](pf_host_config::AudioOutputMode)), which still honours the older
/// `PUNKTFUNK_HOST_AUDIO` spelling.
pub(crate) fn host_audio_requested() -> bool {
    pf_host_config::config()
        .audio_output_mode
        .prefers_host_hardware()
}

/// The operator's default playback/recording devices must not be touched at all — the
/// `follow_default` mode, formerly `PUNKTFUNK_KEEP_DEFAULT`.
pub(crate) fn keep_default_devices() -> bool {
    pf_host_config::config().audio_output_mode.keeps_default()
}

/// One wiring pass plus the inputs the desktop-audio capture loop's failure handling needs:
/// the endpoint-set fingerprint ([`wiring_plan::fingerprint`] — the wait key while a plan is
/// unsatisfiable, snapshotted from the SAME enumeration the plan consumed so a device arriving
/// mid-pass can't leave the waiter keyed to a set the plan never saw) and the render inventory
/// (the one-shot "why is there no loopback" diagnosis).
pub(crate) struct WiredPlan {
    pub wiring: Wiring,
    pub fingerprint: u64,
    pub renders: Vec<Endpoint>,
}

/// Fingerprint of the CURRENT endpoint set (both directions) WITHOUT a wiring pass: enumeration
/// and a hash — no plan, no default-device writes, no logs. This is the cheap poll the capture
/// loop runs while waiting out a failure; the full [`wire_now`] only runs again once this moves.
/// Must run on a COM-initialized thread, like [`wire_now`].
pub(crate) fn endpoint_fingerprint() -> u64 {
    wiring_plan::fingerprint(
        &list_endpoints(Direction::Render),
        &list_endpoints(Direction::Capture),
    )
}

/// [`wire_now_full`] for callers that only need the assignment (the mic paths).
pub(crate) fn wire_now(set_playback: bool) -> Wiring {
    wire_now_full(set_playback).wiring
}

/// Endpoint ids among `renders` that are the host's own pad-audio endpoints — the exclusion
/// data [`plan`] runs on. Detection lives in [`super::pad_endpoint`] (stamped PFDS container /
/// devnode marker, registry-only reads); this is just the per-pass collection.
fn pad_render_ids(renders: &[Endpoint]) -> Vec<String> {
    renders
        .iter()
        .filter(|(_, id)| super::pad_endpoint::is_pad_render_endpoint(id))
        .map(|(_, id)| id.clone())
        .collect()
}

/// Enumerate endpoints, compute the assignment, apply the default-device changes (unless
/// `PUNKTFUNK_KEEP_DEFAULT`), and return the plan for the caller to act on (mic target / loopback
/// echo guard). `set_playback` — true only from the desktop-audio capture open — additionally
/// parks the default PLAYBACK device on the plan's loopback endpoint for the capture's lifetime
/// (the mic pump passes false: it runs while the host is idle and must not silence the box).
/// Must run on a COM-initialized thread (the WASAPI worker threads all `initialize_mta` first).
/// Logged only when the assignment changes, so per-open recomputation stays quiet in the steady
/// state.
pub(crate) fn wire_now_full(set_playback: bool) -> WiredPlan {
    recover_orphaned_default();
    let renders = list_endpoints(Direction::Render);
    let captures = list_endpoints(Direction::Capture);
    let fingerprint = wiring_plan::fingerprint(&renders, &captures);
    let want = std::env::var("PUNKTFUNK_MIC_DEVICE")
        .ok()
        .map(|s| s.to_lowercase());
    // The host's own pad-audio ("DualSense speaker") endpoints, by id — the pure plan filters
    // them out of every role. Identity is platform data (stamped container / devnode marker),
    // so it is collected HERE and passed in, like the candidate lists themselves.
    let pad_ids = pad_render_ids(&renders);
    // Mix formats are read only when we are actually going to park the playback default (i.e. a
    // desktop-audio capture is opening). The mic pump wires on every open while the host is idle
    // and does not care which loopback endpoint wins, so it must not pay an IAudioClient
    // activation per render endpoint on every pass.
    let probe: &dyn Fn(&Endpoint) -> Option<MixFormat> = if set_playback {
        &mix_format_of
    } else {
        &wiring_plan::no_formats
    };
    let wiring = plan_with_formats(
        &renders,
        &captures,
        want.as_deref(),
        host_audio_requested(),
        probe,
        // The loopback is opened at the session's negotiated channel count, but the wiring pass
        // runs before (and outside) any session. Stereo is the floor every session uses and the
        // only count a *narrowing* verdict can be made against without guessing: an endpoint that
        // cannot carry stereo cannot carry 5.1 either.
        2,
        &pad_ids,
    );
    let done = |wiring: Wiring| WiredPlan {
        wiring,
        fingerprint,
        renders: renders.clone(),
    };

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
            loopback_last_resort = wiring.loopback_last_resort,
            renders = ?renders.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            "audio wiring plan"
        );
        // The quality warning the 2026-08-03 report had no way to produce. Says WHICH endpoint,
        // WHY it is narrow, and the two things the operator can actually do about it.
        if let (Some(why), Some((name, _))) = (&wiring.loopback_narrowing, &wiring.loopback_render)
        {
            tracing::warn!(
                device = %name,
                "the desktop-audio loopback endpoint {why} — streamed audio will sound worse \
                 than it does on the host. Attach or select a 48 kHz stereo output device, or \
                 set audio.output_mode = host_and_client (PUNKTFUNK_HOST_AUDIO=1) to prefer \
                 real hardware"
            );
        }
        if wiring.mic_render.is_some() && wiring.loopback_unsatisfiable() {
            // Inventory + per-endpoint reasons + ONLY the remedies not already taken — the old
            // static advice here suggested installing the Steam pair to a field box that had it
            // installed (its Microphone half was exactly what the mic had reserved).
            tracing::warn!(
                "desktop audio unavailable: {}",
                wiring_plan::describe_no_loopback(&renders, &wiring)
            );
        }
    }

    if keep_default_devices() {
        if changed {
            tracing::info!(
                mode = %pf_host_config::config().audio_output_mode.as_str(),
                "audio output mode is follow_default — leaving the audio default devices untouched"
            );
        }
        return done(wiring);
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
            match plan(&renders, &captures, want.as_deref(), true, &pad_ids).loopback_render {
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
        // `set_default_endpoint` is NOT a no-op on an unchanged default: it unconditionally
        // fires SetDefaultEndpoint for all three roles (an audio-policy write plus a
        // device-graph notification, each). Re-asserting on every wiring pass therefore both
        // churned the policy store AND silently stomped an operator's own recording-device
        // choice within one reopen cycle — write only when the plan changed or the default
        // actually drifted off the target.
        if changed || default_capture_id().as_deref() != Some(id.as_str()) {
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
    }
    done(wiring)
}

/// The operator's default playback endpoint while we have it parked on the loopback sink:
/// `(previous_id, id_we_set)`. In-memory source of truth; mirrored to [`park_marker_path`] so a
/// crashed host can't strand the box on the silent sink.
static PARKED: Mutex<Option<(String, String)>> = Mutex::new(None);

/// On-disk crash marker mirroring [`PARKED`] (two lines: previous id, set id).
fn park_marker_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("audio-default.prev")
}

/// The current default RENDER endpoint id, if any. pub(crate): the pad-endpoint provisioning
/// uses it for its default-device guard (a freshly minted pad endpoint must never stay the
/// default playback device).
pub(crate) fn default_render_id() -> Option<String> {
    wasapi::DeviceEnumerator::new()
        .ok()?
        .get_default_device(&Direction::Render)
        .ok()?
        .get_id()
        .ok()
}

/// The current default CAPTURE endpoint id, if any — the recording-side analogue of
/// [`default_render_id`], read before asserting the recording default so an already-correct
/// default costs zero IPolicyConfig writes.
fn default_capture_id() -> Option<String> {
    wasapi::DeviceEnumerator::new()
        .ok()?
        .get_default_device(&Direction::Capture)
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

/// Put the default playback device back on the endpoint we are already capturing, WITHOUT a
/// wiring pass (WP2.4).
///
/// The capture loop uses this when something else takes the default mid-stream: in Assert mode the
/// capture is bound to the planned endpoint explicitly, so the only thing a hijacked default
/// changes is where *apps* render — one `IPolicyConfig` write fixes that, where the old path tore
/// the capture down and re-ran the whole wiring pass. Deliberately does not touch the [`PARKED`]
/// memo: the endpoint is the one we already parked, so the operator's original default is
/// unchanged and still owed back at stream end.
pub(crate) fn reassert_default_playback(id: &str) -> bool {
    match set_default_endpoint(id) {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "failed to re-assert the default playback device");
            false
        }
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
///
/// Resolves through [`super::pad_endpoint::open_wasapi_device`], NOT the `wasapi` crate's
/// `DeviceEnumerator::get_device` — that one hands `GetDevice` a freed string (see the helper's
/// docs), so it fails at random on ids that are perfectly valid.
pub(crate) fn open_endpoint(ep: &Endpoint) -> Result<wasapi::Device> {
    super::pad_endpoint::open_wasapi_device(&ep.1)
        .map_err(|e| anyhow!("open endpoint {:?}: {e:#}", ep.0))
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

// This mirrors the vtable of the UNDOCUMENTED `IPolicyConfig` COM interface, so there is no header
// to check it against and no `windows-rs` binding to fall back on. `set_default_endpoint` is called
// by INDEX — `((*vtbl).set_default_endpoint)(..)` is really "the 14th function pointer in this
// table" — so a field added, removed or resized above it does not fail to compile: it silently calls
// a DIFFERENT function through a mismatched signature, which is arbitrary-code territory rather
// than a wrong answer. The `_reserved` gap is what makes that easy to get wrong, since its ten slots
// carry no names to anchor a review. These assertions pin the two things the call actually depends
// on: the slot index of `set_default_endpoint`, and the size of the table up to it.
const _: () = {
    use std::mem::{offset_of, size_of};
    type P = *const c_void;
    // 3 IUnknown slots + 10 reserved = `set_default_endpoint` is slot 13 (0-based).
    assert!(offset_of!(IPolicyConfigVtbl, query_interface) == 0);
    assert!(offset_of!(IPolicyConfigVtbl, add_ref) == size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, release) == 2 * size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, _reserved) == 3 * size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, set_default_endpoint) == 13 * size_of::<P>());
    assert!(size_of::<IPolicyConfigVtbl>() == 14 * size_of::<P>());
};

/// Set `device_id` as the default audio endpoint for eConsole/eMultimedia/eCommunications via the
/// undocumented `IPolicyConfig::SetDefaultEndpoint` (the call `mmsys.cpl` makes). Errs if any role
/// fails. pub(crate): the pad-endpoint default-device guard restores the operator's default
/// through the same machinery.
pub(crate) fn set_default_endpoint(device_id: &str) -> Result<()> {
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
