//! Windows audio device auto-wiring — production mic + desktop-audio passthrough with zero manual
//! setup.
//!
//! A headless host has no real audio output, so BOTH the desktop-audio loopback ([`super::wasapi_cap`])
//! and the virtual mic ([`super::wasapi_mic`]) must run on VIRTUAL audio cables — and on DIFFERENT
//! ones, or the loopback re-captures the injected mic (an infinite echo). The host mints its own
//! endpoint pair from Steam's streaming drivers (see [`super::minted`] — the plan's tier-0); the
//! name-based ladder below covers boxes where minting is unavailable. Historically the installer
//! bundled
//! VB-Audio Virtual Cable (the mic target: its "CABLE Input" render endpoint → "CABLE Output" capture)
//! and the host auto-installs the Steam Streaming pair (a loopback-capable render). This module wires
//! them up so no manual Sound-settings fiddling is ever needed:
//!
//! * the **mic inject target** is assigned FIRST (VB-Cable "CABLE Input" preferred) — mic passthrough
//!   is what the cable is bundled for, so it wins the cable even when the cable is the only render
//!   endpoint on the box (the loopback then reports itself unavailable instead of echoing). One
//!   exception: the Steam Streaming Microphone is surrendered to the loopback when taking it would
//!   leave desktop audio on the known-silent last resort or nothing — game audio outranks the mic
//!   (see [`wiring_plan`], `Wiring::mic_withheld`);
//! * default **PLAYBACK** → the plan's loopback endpoint, applied ONLY while a desktop-audio capture
//!   is open (`set_playback` — the mic pump must never park the playback default while the host is
//!   idle). By default that endpoint is the SILENT sink (Steam Streaming Microphone render side) so
//!   audio plays on the client only; `audio.output_mode = host_and_client` (formerly
//!   `PUNKTFUNK_HOST_AUDIO`) prefers real hardware instead (audible on both ends). Since 2026-08 a
//!   silent sink must also be able to CARRY the mix — one that narrows it (a voice-carrier endpoint
//!   mixing mono or at 24 kHz) loses to real hardware; see [`super::wiring_plan`]. **Never** the
//!   Steam Streaming Speakers, whose loopback is silent — validated live;
//! * default **RECORDING** → the mic target's capture endpoint (VB-Cable "CABLE Output") so host apps
//!   record the client's mic by default — applied, like the playback default, ONLY while a
//!   desktop-audio capture is open. It used to be asserted on EVERY wiring pass, mic pump at boot
//!   included, which left an IDLE box's default recording/communication device parked on a virtual
//!   microphone nothing feeds — and games bind the default microphone at launch (`SetDefaultEndpoint`
//!   covers eCommunications, so in-game voice binds it too). The 2026-08 Helldivers 2 field reports
//!   measured that as 1% lows of 2–5 FPS in a LOCALLY played game while the host sat idle (HD2 is
//!   Wwise + always-on voice, exactly the "finicky with audio devices" case its own wiki warns
//!   about). An idle host must leave the box's audio defaults exactly as the operator set them.
//!
//! Because both defaults are *parked* during a stream — playback on a silent sink, recording on the
//! virtual mic — the operator's devices are remembered ([`park_default_playback`] /
//! [`park_default_recording`], plus on-disk crash markers) and put back when the capture closes
//! ([`restore_default_playback`] / [`restore_default_recording`]) or, after a crash, on the next
//! process's first wiring pass — an operator must never be stranded with silent speakers or a dead
//! mic. A default the operator changed themselves mid-stream is respected (no restore over their
//! choice).
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
pub(crate) fn wire_now(park_defaults: bool) -> Wiring {
    wire_now_full(park_defaults).wiring
}

/// The most recent wiring verdict, as the LAST wiring pass computed it (the mic pump wires
/// eagerly at host start and on every reopen, so this is fresh in the steady state). Change
/// detection for the once-per-change log lives on the same cell.
static LAST_WIRING: Mutex<Option<Wiring>> = Mutex::new(None);

/// Read-only snapshot of [`LAST_WIRING`] for the status API — never triggers a wiring pass
/// (a pass does COM work and IPolicyConfig writes; a status poll must do neither).
pub(crate) fn last_wiring() -> Option<Wiring> {
    LAST_WIRING.lock().unwrap().clone()
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
/// echo guard). `park_defaults` — true only from the desktop-audio capture open — additionally
/// parks the default PLAYBACK device on the plan's loopback endpoint and the default RECORDING
/// device on the virtual mic's capture side, both for the capture's lifetime (the mic pump passes
/// false: it runs while the host is idle and must neither silence the box nor hold its default
/// microphone — the idle-parked recording default is the 2026-08 Helldivers 2 tank, see the
/// module docs). Must run on a COM-initialized thread (the WASAPI worker threads all
/// `initialize_mta` first). Logged only when the assignment changes, so per-open recomputation
/// stays quiet in the steady state.
pub(crate) fn wire_now_full(park_defaults: bool) -> WiredPlan {
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
    // Mix formats are read only when we are actually going to park the defaults (i.e. a
    // desktop-audio capture is opening). The mic pump wires on every open while the host is idle
    // and does not care which loopback endpoint wins, so it must not pay an IAudioClient
    // activation per render endpoint on every pass.
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
        // The loopback is opened at the session's negotiated channel count, but the wiring pass
        // runs before (and outside) any session. Stereo is the floor every session uses and the
        // only count a *narrowing* verdict can be made against without guessing: an endpoint that
        // cannot carry stereo cannot carry 5.1 either.
        2,
        &pad_ids,
        // The minted "Punktfunk Speakers/Microphone" ids — tier-0 identity, empty until the
        // provider latches. The ensure hook makes a box where Steam arrives later mint on a
        // wiring pass instead of at the next reboot (cheap once latched; cooled-down retries
        // while not).
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

    // Log assignment changes exactly once (first plan included).
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
    // Recording-default hygiene, IDLE passes only: builds before 2026-08-14 parked the default
    // recording on the virtual mic on EVERY wiring pass (boot included) and recorded nothing to
    // restore — so an upgraded box would otherwise sit wedged on a microphone nothing feeds
    // until the operator noticed (the Helldivers 2 idle tank; the session-scoped park below
    // can't heal it either: it remembers a previous default only when the default isn't already
    // ours). While nothing is parked, a default found sitting on the plan's mic capture moves to
    // the first real microphone. Session passes own the default and are exempt; a box with no
    // real microphone is left alone.
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
        // The recording default is SESSION-SCOPED like the playback default, and for the same
        // reason inverted: parking it while idle handed the box's default microphone (and, via
        // eCommunications, every game's voice input) to a virtual mic nothing feeds — the
        // 2026-08 Helldivers 2 idle tank (see the module docs). A game launched DURING the
        // stream still binds the client's mic (this runs before the session's game does);
        // one launched before the stream keeps the operator's mic, which is the honest answer.
        if let Some((name, id)) = &wiring.mic_capture {
            park_default_recording(name, id, changed);
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

/// The operator's default recording endpoint while we have it parked on the virtual mic:
/// `(previous_id, id_we_set)` — the recording-side twin of [`PARKED`].
static PARKED_REC: Mutex<Option<(String, String)>> = Mutex::new(None);

/// On-disk crash marker mirroring [`PARKED_REC`] (two lines: previous id, set id).
fn rec_marker_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("audio-default-rec.prev")
}

/// Consume a park marker file: returns the PREVIOUS default's id when the marker existed AND the
/// current default still is the endpoint we set — a default the operator changed since wins, like
/// on every other restore path. The file is removed either way (it describes a park that is over).
fn take_marker(path: &std::path::Path, current_default: Option<String>) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path);
    let mut lines = s.lines();
    let (prev, set) = (lines.next()?, lines.next()?);
    (current_default.as_deref() == Some(set)).then(|| prev.to_string())
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
pub(crate) fn default_capture_id() -> Option<String> {
    wasapi::DeviceEnumerator::new()
        .ok()?
        .get_default_device(&Direction::Capture)
        .ok()?
        .get_id()
        .ok()
}

/// Once per process: if a crash marker from a previous run exists, the host died while a default
/// (playback and/or recording) was parked — put the operator's device back, but only if the
/// default still IS the endpoint we set (a manual change since the crash wins). Runs on the first
/// wiring pass (the mic pump wires eagerly at host start, so this fires at boot, not at the first
/// stream).
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

/// [`recover_orphaned_default`]'s uninstall-time twin: same "put the operator's device back if
/// the default is still parked on ours" rule, minus the `Once` gate (the uninstaller is a fresh
/// process that runs it exactly once) — and it always drops the marker file, because there is no
/// next host run to consume it.
///
/// Why the uninstaller needs this at all: the devnode sweep that follows deletes the endpoint the
/// default may still point at. Windows would then re-pick something on its own, but it re-picks by
/// its OWN ranking, not the device the operator had before we parked it. Restoring first means
/// uninstalling gives the box back exactly the default it came with.
///
/// Returns whether a device was actually put back — the caller only logs it.
pub(crate) fn unpark_default_for_uninstall() -> bool {
    let mut restored = false;
    for (path, current) in [
        (park_marker_path(), default_render_id()),
        (rec_marker_path(), default_capture_id()),
    ] {
        // A default the operator changed by hand since the park wins, exactly as on the
        // recovery path (`take_marker` answers None then).
        if let Some(prev) = take_marker(&path, current) {
            restored |= set_default_endpoint(&prev).is_ok();
        }
    }
    restored
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

/// Make `id` the default recording device for the duration of the desktop-audio capture —
/// [`park_default_playback`]'s recording twin, remembering the operator's current default (in
/// memory + the crash marker) the FIRST time so [`restore_default_recording`] can put it back.
/// Nothing is remembered when `id` already is the default — there is nothing to restore.
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
            // Re-park onto a different endpoint mid-stream (plan changed): keep the ORIGINAL
            // previous default, update what we set.
            Some((prev, set)) if set != id => {
                let _ = std::fs::write(rec_marker_path(), format!("{prev}\n{id}"));
                *set = id.to_string();
            }
            Some(_) => {}
        }
    }
    // `set_default_endpoint` is NOT a no-op on an unchanged default: it unconditionally fires
    // SetDefaultEndpoint for all three roles (an audio-policy write plus a device-graph
    // notification, each) — write only when the plan changed or the default actually drifted
    // off the target, or the policy store churns on every reopen.
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

/// Put the operator's default recording device back after streaming — the inverse of
/// [`park_default_recording`], with [`restore_default_playback`]'s exact rules: no-op if we never
/// parked it, and a default the operator changed themselves mid-stream is left alone. Must run on
/// a COM-initialized thread (called from the capture thread's exit path).
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

/// Open a device by endpoint id, with a name for error context.
///
/// Resolves through [`super::pad_endpoint::open_wasapi_device`] rather than the `wasapi` crate's
/// `DeviceEnumerator::get_device`: that one handed `GetDevice` a freed string through 0.23, so it
/// failed at random on ids that are perfectly valid. `wasapi 0.24` fixed that, but we keep the one
/// resolution path — see the helper's docs.
pub(crate) fn open_endpoint(ep: &Endpoint) -> Result<wasapi::Device> {
    super::pad_endpoint::open_wasapi_device(&ep.1)
        .map_err(|e| anyhow!("open endpoint {:?}: {e:#}", ep.0))
}

// --- IPolicyConfig (undocumented): default-endpoint and endpoint-visibility writes. ---

/// The `IPolicyConfig` vtable. Only `SetDefaultEndpoint` and `SetEndpointVisibility` are called;
/// the 10 methods between `Release` and them (`GetMixFormat` … `SetPropertyValue`) are
/// placeholders so the slot offsets are correct.
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

// This mirrors the vtable of the UNDOCUMENTED `IPolicyConfig` COM interface, so there is no header
// to check it against and no `windows-rs` binding to fall back on. `set_default_endpoint` is called
// by INDEX — `((*vtbl).set_default_endpoint)(..)` is really "the 14th function pointer in this
// table" — so a field added, removed or resized above it does not fail to compile: it silently calls
// a DIFFERENT function through a mismatched signature, which is arbitrary-code territory rather
// than a wrong answer. The `_reserved` gap is what makes that easy to get wrong, since its ten slots
// carry no names to anchor a review. These assertions pin the things the calls actually depend
// on: the slot indexes of `set_default_endpoint` and `set_endpoint_visibility`, and the size of
// the table up to them.
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

/// Show or hide an audio endpoint via the undocumented `IPolicyConfig::SetEndpointVisibility` —
/// the exact call behind mmsys.cpl's "Disable"/"Enable" device menu. A hidden endpoint drops to
/// `DEVICE_STATE_DISABLED`: it vanishes from every ACTIVE enumeration and cannot be opened, but
/// its devnode, driver binding and stamped identity all stay put — showing it again is instant
/// and raises no PnP traffic. pub(crate): the pad-endpoint provider parks its "Wireless
/// Controller" speaker hidden while no client pad is attached (a visible idle pad speaker makes
/// libScePad titles engage their DualSense-haptics path against an endpoint nothing services —
/// the 2026-08-14 Helldivers 2 field confirmation).
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
