//! Windows audio auto-wiring: virtual-mic inject plus desktop-audio loopback.
//!
//! The two jobs must land on different endpoints — WASAPI loopback recaptures
//! whatever the mic writes, so sharing a cable is an infinite echo. Assignment
//! lives in the pure [`wiring_plan`](super::wiring_plan) module; this crate
//! enumerates, applies the plan, and logs. [`wire_now`] runs on every mic/capture
//! (re)open because endpoints churn (boot registration, hotplug, driver installs).
//!
//! Playback and recording defaults park only while a desktop-audio capture is
//! open (the idle mic pump must not silence speakers or steal the default mic).
//! The operator's devices are remembered in memory plus on-disk crash markers
//! ([`park_default_playback`] / [`park_default_recording`]) and restored when
//! capture closes or on the next process's first wiring pass. A default the
//! operator changed mid-stream is left alone.
//!
//! Default writes go through undocumented `IPolicyConfig` (the call `mmsys.cpl`
//! makes; neither crate exposes it). `audio.output_mode = follow_default` still
//! computes the plan — the mic needs a target — but skips the default writes.

use super::wiring_plan::{self, plan, plan_with_formats, Endpoint, MixFormat, Wiring};
use anyhow::{anyhow, bail, Result};
use std::ffi::c_void;
use std::sync::Mutex;
use wasapi::Direction;

/// Engine mix format of a render endpoint, or `None` if it cannot be asked.
///
/// Shared-mode capture requests 48 kHz f32 with autoconvert, so WASAPI will
/// silently downmix a voice-carrier (mono / 24 kHz). One `IAudioClient`
/// activation per endpoint, only during a wiring pass. Every failure maps to
/// `None`: the plan treats unknown as non-narrowing, matching pre-format boxes.
pub(crate) fn mix_format_of(ep: &Endpoint) -> Option<MixFormat> {
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

/// Engine rate the desktop-audio loopback would capture at, answered before
/// `Welcome` and without opening a capture stream.
///
/// Shared-mode `IAudioClient` with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` will
/// interpolate a 96 kHz request on a 48 kHz engine and return success — so the
/// number that matters is [`mix_format_of`]. An operator who wants 96 kHz sets
/// the endpoint's own rate in Windows; the host then sees it here.
///
/// Read-only: enumerates, runs the pure [`plan_with_formats`], reads one mix
/// format. Not [`wire_now_full`] — that parks defaults, mints, and logs, none
/// of which may happen for a session that is about to resolve to Opus.
///
/// Plan inputs MUST stay in lockstep with [`wire_now_full`]: a divergence here
/// reads the format of a device we do not capture. The real probe (not
/// [`wiring_plan::no_formats`]) is load-bearing — narrowing demotes a candidate
/// below real hardware. Every failure is [`CaptureRate::Unknown`] (decline):
/// unlike the wiring plan, unknown here means we cannot prove the label.
///
/// Must run on a COM-initialized thread; initializes MTA because the caller is
/// a tokio blocking-pool thread. A repeat init returns `S_FALSE` (success).
pub(crate) fn probe_capture_rate() -> super::CaptureRate {
    if let Err(e) = wasapi::initialize_mta().ok() {
        tracing::debug!(error = %e, "hi-res capture-rate probe: CoInitializeEx (MTA) failed");
        return super::CaptureRate::Unknown;
    }
    let renders = list_endpoints(Direction::Render);
    let captures = list_endpoints(Direction::Capture);
    let want = std::env::var("PUNKTFUNK_MIC_DEVICE")
        .ok()
        .map(|s| s.to_lowercase());
    let pad_ids = pad_render_ids(&renders);
    let wiring = plan_with_formats(
        &renders,
        &captures,
        want.as_deref(),
        host_audio_requested(),
        &mix_format_of,
        // Stereo: the only count hi-res carries, and the same floor `wire_now_full` plans against.
        2,
        &pad_ids,
        &super::minted::minted_ids(),
    );
    let Some(ep) = wiring.loopback_render else {
        tracing::debug!("hi-res capture-rate probe: no desktop-audio loopback endpoint is planned");
        return super::CaptureRate::Unknown;
    };
    // `plan_with_formats` ranked on this format but did not return the numbers.
    // Caching through it to save one `GetMixFormat` would add a second way to disagree.
    match mix_format_of(&ep) {
        Some(f) => super::CaptureRate::Engine(f.rate_hz),
        None => {
            tracing::debug!(device = %ep.0,
                "hi-res capture-rate probe: the planned loopback endpoint would not report its \
                 mix format");
            super::CaptureRate::Unknown
        }
    }
}

/// Active endpoints only (`friendly_name`, `endpoint_id`).
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

/// True when the loopback plan must prefer real hardware over the silent sink.
pub(crate) fn host_audio_requested() -> bool {
    pf_host_config::config()
        .audio_output_mode
        .prefers_host_hardware()
}

/// Skip default-device writes: `follow_default` mode, or a session's keep-host-audio ask.
pub(crate) fn keep_default_devices() -> bool {
    pf_host_config::config().audio_output_mode.keeps_default()
        || crate::audio::capture_policy::session_keeps_default()
}

/// One wiring pass: assignment, fingerprint of the same enumeration the plan consumed
/// (a device arriving mid-pass must not key the waiter to a set the plan never saw),
/// and the render inventory for the no-loopback diagnosis.
pub(crate) struct WiredPlan {
    pub wiring: Wiring,
    pub fingerprint: u64,
    pub renders: Vec<Endpoint>,
}

/// Endpoint-set hash with no plan, no default writes, no logs. Cheap poll while a
/// capture waits out a failure; [`wire_now`] runs again only once this moves.
/// Must run on a COM-initialized thread.
pub(crate) fn endpoint_fingerprint() -> u64 {
    wiring_plan::fingerprint(
        &list_endpoints(Direction::Render),
        &list_endpoints(Direction::Capture),
    )
}

pub(crate) fn wire_now(park_defaults: bool) -> Wiring {
    wire_now_full(park_defaults).wiring
}

/// Last wiring verdict. Change detection for the once-per-change log lives here.
static LAST_WIRING: Mutex<Option<Wiring>> = Mutex::new(None);

/// Snapshot of [`LAST_WIRING`]. A status poll must not run COM or IPolicyConfig writes.
pub(crate) fn last_wiring() -> Option<Wiring> {
    LAST_WIRING.lock().unwrap().clone()
}

/// Pad-audio render ids among `renders` — exclusion data [`plan`] consumes.
/// Identity lives in [`super::pad_endpoint`]; this is the per-pass collection.
fn pad_render_ids(renders: &[Endpoint]) -> Vec<String> {
    renders
        .iter()
        .filter(|(_, id)| super::pad_endpoint::is_pad_render_endpoint(id))
        .map(|(_, id)| id.clone())
        .collect()
}

/// Enumerate, plan, apply default-device writes (unless `follow_default`), return the
/// assignment. `park_defaults` is true only from desktop-audio capture open: that parks
/// playback on the loopback sink and recording on the virtual mic. The idle mic pump
/// passes false — it must neither silence speakers nor steal the default microphone.
/// COM-initialized thread. Logged only when the assignment changes.
pub(crate) fn wire_now_full(park_defaults: bool) -> WiredPlan {
    recover_orphaned_default();
    let renders = list_endpoints(Direction::Render);
    let captures = list_endpoints(Direction::Capture);
    let fingerprint = wiring_plan::fingerprint(&renders, &captures);
    let want = std::env::var("PUNKTFUNK_MIC_DEVICE")
        .ok()
        .map(|s| s.to_lowercase());
    // Pad-audio ids: platform identity, collected here and passed into the pure plan
    // like the candidate lists — the plan filters them out of every role.
    let pad_ids = pad_render_ids(&renders);
    // Mix formats only when parking defaults. The idle mic pump does not care which
    // loopback wins and must not activate an IAudioClient per render on every pass.
    let probe: &dyn Fn(&Endpoint) -> Option<MixFormat> = if park_defaults {
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
        // Stereo: the floor every session uses, and the only count a narrowing verdict
        // can be made against without a session. Cannot-carry-stereo cannot-carry-5.1.
        2,
        &pad_ids,
        // Minted Speakers/Microphone ids — empty until the provider latches. `ensure`
        // here mints when Steam arrives mid-run instead of waiting for the next reboot.
        &{
            super::minted::ensure_provisioned();
            super::minted::minted_ids()
        },
    );
    let done = |wiring: Wiring| WiredPlan {
        wiring,
        fingerprint,
        renders: renders.clone(),
    };

    let changed = {
        let mut last = LAST_WIRING.lock().unwrap();
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
            mic_withheld = wiring.mic_withheld,
            readiness = ?wiring_plan::readiness(&wiring),
            renders = ?renders.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            "audio wiring plan"
        );
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
            // Per-endpoint reasons plus only unused remedies — static "install Steam" advice
            // is wrong when the Microphone half is already the mic reservation.
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
                session_asked = crate::audio::capture_policy::session_keeps_default(),
                "leaving the audio default devices untouched (follow_default mode, or a \
                 session's keep-host-audio ask)"
            );
        }
        return done(wiring);
    }
    // If the default render is the mic target, apps render into the virtual mic.
    // Move it first: a parked `prev` that is the cable would restore that after a stream.
    if let Some((mic_name, mic_id)) = &wiring.mic_render {
        if default_render_id().as_deref() == Some(mic_id.as_str()) {
            // host_audio plan: real hardware first, so the new default is audible.
            match plan(
                &renders,
                &captures,
                want.as_deref(),
                true,
                &pad_ids,
                &super::minted::minted_ids(),
            )
            .loopback_render
            {
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
    // Idle, nothing parked: default on the virtual mic moves to a real microphone.
    // Session park cannot heal this — it only remembers a prev that is not already ours.
    if !park_defaults && PARKED_REC.lock().unwrap().is_none() {
        if let Some((mic_name, mic_id)) = &wiring.mic_capture {
            if default_capture_id().as_deref() == Some(mic_id.as_str()) {
                if let Some((name, id)) =
                    wiring_plan::real_capture(&captures, Some(mic_id.as_str()))
                {
                    match set_default_endpoint(id) {
                        Ok(()) => tracing::info!(from = %mic_name, device = %name,
                            "default recording was left on the virtual mic outside a stream — \
                             moved it back to a real microphone"),
                        Err(e) => tracing::warn!(device = %name, error = %format!("{e:#}"),
                            "failed to move the default recording off the virtual mic"),
                    }
                }
            }
        }
    }
    if park_defaults {
        if let Some((name, id)) = &wiring.loopback_render {
            let mic_id = wiring.mic_render.as_ref().map(|(_, m)| m.as_str());
            park_default_playback(name, id, changed, mic_id);
        }
        // Recording park is session-scoped: idle park hands the default mic (and, via
        // eCommunications, in-game voice) to a virtual mic nothing feeds.
        if let Some((name, id)) = &wiring.mic_capture {
            park_default_recording(name, id, changed);
        }
    }
    done(wiring)
}

/// Parked playback default: `(previous_id, id_we_set)`. In-memory source of truth,
/// mirrored to [`park_marker_path`] so a crash cannot leave the box on the silent sink.
static PARKED: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Crash marker for [`PARKED`]: two lines, previous id then set id.
fn park_marker_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("audio-default.prev")
}

/// Parked recording default: `(previous_id, id_we_set)`. Twin of [`PARKED`].
static PARKED_REC: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Crash marker for [`PARKED_REC`]: two lines, previous id then set id.
fn rec_marker_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("audio-default-rec.prev")
}

/// Consume a park marker. Returns the previous id only if the current default is still
/// the endpoint we set (an operator change since wins). The file is removed either way.
fn take_marker(path: &std::path::Path, current_default: Option<String>) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path);
    let mut lines = s.lines();
    let (prev, set) = (lines.next()?, lines.next()?);
    (current_default.as_deref() == Some(set)).then(|| prev.to_string())
}

/// Current default render endpoint id. Pad-endpoint provisioning uses this so a
/// freshly minted pad speaker never stays the default playback device.
pub(crate) fn default_render_id() -> Option<String> {
    wasapi::DeviceEnumerator::new()
        .ok()?
        .get_default_device(&Direction::Render)
        .ok()?
        .get_id()
        .ok()
}

/// Current default capture endpoint id. Read before asserting so an already-correct
/// default costs zero IPolicyConfig writes.
pub(crate) fn default_capture_id() -> Option<String> {
    wasapi::DeviceEnumerator::new()
        .ok()?
        .get_default_device(&Direction::Capture)
        .ok()?
        .get_id()
        .ok()
}

/// Once per process: restore a default a previous run left parked, only if it is still
/// the endpoint we set. Runs on the first wiring pass (mic pump at host start), not the
/// first stream.
fn recover_orphaned_default() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for (path, current, what) in [
            (park_marker_path(), default_render_id(), "playback"),
            (rec_marker_path(), default_capture_id(), "recording"),
        ] {
            let Some(prev) = take_marker(&path, current) else {
                continue;
            };
            match set_default_endpoint(&prev) {
                Ok(()) => tracing::info!(
                    "restored the default {what} device a previous host run left parked"
                ),
                Err(e) => tracing::warn!(error = %format!("{e:#}"),
                    "failed to restore the default {what} device left by a previous run"),
            }
        }
    });
}

/// Uninstall twin of [`recover_orphaned_default`]: restore if still parked on ours,
/// then always drop the marker (no next host run consumes it). No `Once` — the
/// uninstaller is a fresh process. Must run before the devnode sweep: Windows would
/// otherwise re-pick by its own ranking, not the operator's pre-park device.
///
/// Returns whether a device was actually put back.
pub(crate) fn unpark_default_for_uninstall() -> bool {
    let mut restored = false;
    for (path, current) in [
        (park_marker_path(), default_render_id()),
        (rec_marker_path(), default_capture_id()),
    ] {
        if let Some(prev) = take_marker(&path, current) {
            restored |= set_default_endpoint(&prev).is_ok();
        }
    }
    restored
}

/// Park playback on `id` for the capture's life. Remembers the operator default the
/// first time (memory + crash marker). Nothing remembered if `id` already is default.
/// The mic target is never stored as `prev` — restoring it would feed the virtual mic.
/// Guards the race the hygiene pass in [`wire_now`] usually already closed.
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
            // Plan changed mid-stream: keep the original previous default, update what we set.
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

/// Park recording on `id` for the capture's life — [`park_default_playback`]'s twin.
/// Remembers the operator default the first time; nothing if `id` already is default.
fn park_default_recording(name: &str, id: &str, changed: bool) {
    let cur = default_capture_id();
    if cur.as_deref() != Some(id) {
        let mut parked = PARKED_REC.lock().unwrap();
        match parked.as_mut() {
            None => {
                if let Some(prev) = cur.clone() {
                    let _ = std::fs::write(rec_marker_path(), format!("{prev}\n{id}"));
                    *parked = Some((prev, id.to_string()));
                }
            }
            // Plan changed mid-stream: keep the original previous default, update what we set.
            Some((prev, set)) if set != id => {
                let _ = std::fs::write(rec_marker_path(), format!("{prev}\n{id}"));
                *set = id.to_string();
            }
            Some(_) => {}
        }
    }
    // `set_default_endpoint` is not a no-op on an unchanged default: it fires
    // SetDefaultEndpoint for all three roles. Write only when the plan changed or
    // the default drifted, or the policy store churns on every reopen.
    if changed || cur.as_deref() != Some(id) {
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

/// Re-set the default playback to the endpoint we are already capturing, without a
/// wiring pass. One `IPolicyConfig` write: the capture is bound explicitly, so a
/// hijacked default only moves where apps render. Does not touch [`PARKED`] — the
/// operator's original default is still owed back at stream end.
pub(crate) fn reassert_default_playback(id: &str) -> bool {
    match set_default_endpoint(id) {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "failed to re-assert the default playback device");
            false
        }
    }
}

/// Inverse of [`park_default_playback`]. No-op if never parked; an operator change
/// mid-stream wins. COM-initialized thread (capture exit path).
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

/// Inverse of [`park_default_recording`]. Same rules as [`restore_default_playback`].
pub(crate) fn restore_default_recording() {
    let Some((prev, set)) = PARKED_REC.lock().unwrap().take() else {
        return;
    };
    let _ = std::fs::remove_file(rec_marker_path());
    if default_capture_id().as_deref() != Some(set.as_str()) {
        return;
    }
    match set_default_endpoint(&prev) {
        Ok(()) => tracing::info!("default recording device restored after streaming"),
        Err(e) => tracing::warn!(error = %format!("{e:#}"),
            "failed to restore the default recording device after streaming"),
    }
}

/// Open by endpoint id. Goes through [`super::pad_endpoint::open_wasapi_device`]
/// so every caller shares one resolution path (see that helper).
pub(crate) fn open_endpoint(ep: &Endpoint) -> Result<wasapi::Device> {
    super::pad_endpoint::open_wasapi_device(&ep.1)
        .map_err(|e| anyhow!("open endpoint {:?}: {e:#}", ep.0))
}

// Undocumented IPolicyConfig: default-endpoint and endpoint-visibility writes.

/// `IPolicyConfig` vtable. Only `SetDefaultEndpoint` and `SetEndpointVisibility`
/// are called; the 10 methods between `Release` and them are placeholders so the
/// slot offsets stay correct.
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
    set_endpoint_visibility: unsafe extern "system" fn(
        *mut c_void,
        windows::core::PCWSTR,
        i32,
    ) -> windows::core::HRESULT,
}

// Mirrors undocumented `IPolicyConfig`: there is no header. Calls go by slot
// index, so a field added above `set_default_endpoint` compiles and invokes a
// different function. These asserts pin the slot indexes and the table size.
const _: () = {
    use std::mem::{offset_of, size_of};
    type P = *const c_void;
    // 3 IUnknown slots + 10 reserved = `set_default_endpoint` is slot 13 (0-based),
    // `set_endpoint_visibility` the slot after.
    assert!(offset_of!(IPolicyConfigVtbl, query_interface) == 0);
    assert!(offset_of!(IPolicyConfigVtbl, add_ref) == size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, release) == 2 * size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, _reserved) == 3 * size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, set_default_endpoint) == 13 * size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, set_endpoint_visibility) == 14 * size_of::<P>());
    assert!(size_of::<IPolicyConfigVtbl>() == 15 * size_of::<P>());
};

/// Set `device_id` as default for eConsole/eMultimedia/eCommunications via
/// `IPolicyConfig::SetDefaultEndpoint`. Errs if any role fails.
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

/// Show or hide an endpoint via `IPolicyConfig::SetEndpointVisibility`. Hidden
/// means `DEVICE_STATE_DISABLED`: gone from ACTIVE enumeration, cannot be opened,
/// but the devnode, driver, and stamped identity stay — showing it again is not
/// a PnP reinstall. Pad-endpoint provider hides the idle DualSense speaker so
/// libScePad titles do not take the haptics path against an unserviced endpoint.
pub(crate) fn set_endpoint_visibility(device_id: &str, visible: bool) -> Result<()> {
    use windows::core::{IUnknown, Interface, GUID, PCWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
    const IID_IPOLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: same contract as `set_default_endpoint` — owned IUnknown from CoCreateInstance,
    // QI'd pointer checked non-null, the call goes through the assertion-pinned vtable slot with
    // a NUL-terminated UTF-16 id and an INT bool, and the QI'd pointer is Released before return.
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
        let hr = ((*vtbl).set_endpoint_visibility)(raw, PCWSTR(wide.as_ptr()), visible as i32);
        ((*vtbl).release)(raw);
        hr.ok()
            .map_err(|e| anyhow!("SetEndpointVisibility({visible}): {e}"))
    }
}
