//! Windows audio endpoint assignment — the PURE planning logic behind
//! [`audio_control`](super::audio_control), split out so it compiles (and its unit tests run) on
//! every platform: the precedence rules here encode the hard-won field knowledge, and regressing
//! them must fail CI on Linux too, not only on a Windows box.
//!
//! Two jobs share the render endpoints and must never collide:
//!
//! * the **virtual mic** writes the client's decoded mic PCM into a virtual cable's render
//!   endpoint (its capture side surfaces as a host microphone), and
//! * the **desktop-audio loopback** captures a render endpoint's mix for the host→client
//!   audio stream.
//!
//! WASAPI loopback captures *everything* an endpoint renders — including what the virtual mic
//! writes — so if both land on the same device the client's voice echoes straight back into the
//! client's own audio stream. The plan therefore assigns the mic its endpoint FIRST (VB-CABLE is
//! bundled by the installer for exactly this) and gives the loopback a *different* one; when only
//! the cable exists (headless box, no other output), the MIC wins and the loopback is honestly
//! unavailable. The old code did the opposite — the mic refused the cable because it was the
//! default render endpoint — which permanently killed mic passthrough in the exact configuration
//! the installer ships (VB-CABLE as the only render device).
//!
//! **Loopback preference depends on where the audio should be heard.** The default is
//! *client-only*: prefer a render endpoint that is silent on the host but has a WORKING loopback
//! (the Steam Streaming *Microphone*'s render side — validated live; the Steam Streaming
//! *Speakers*' loopback is silent) so the desktop mix reaches the stream without also blasting
//! out of the host's speakers. Real hardware is the fallback (audio then plays on both ends).
//! With `host_audio` (the `PUNKTFUNK_HOST_AUDIO` opt-in) the order flips back: real hardware
//! first, so the operator hears the stream locally.

/// A `(friendly_name, endpoint_id)` pair as enumerated from WASAPI.
pub(crate) type Endpoint = (String, String);

/// The coherent endpoint assignment for one wiring pass. Computed fresh on every mic/capture
/// (re)open — Windows endpoints churn (boot-time registration, hotplug, driver installs), so a
/// once-per-process plan goes stale.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wiring {
    /// Render endpoint RESERVED for the virtual mic (the write target). The loopback must never
    /// capture this device.
    pub mic_render: Option<Endpoint>,
    /// The mic device's CAPTURE side — host apps record this; made the default recording device.
    pub mic_capture: Option<Endpoint>,
    /// Render endpoint for the desktop-audio loopback; made the default playback device.
    pub loopback_render: Option<Endpoint>,
}

/// Render-endpoint friendly-name substrings (lowercased) usable as the virtual-mic write target,
/// ordered by preference. VB-CABLE first: the installer bundles it for this exact purpose.
const MIC_CANDIDATES: &[&str] = &[
    "cable input", // VB-Audio Virtual Cable — bundled by the installer
    "steam streaming microphone",
    "voicemeeter input",
    "voicemeeter aux input",
    "virtual",
];

/// `(mic render substring, matching capture substring)` — which capture endpoint surfaces the
/// audio written to a given mic render target.
fn capture_for(mic_render_lname: &str) -> &'static [&'static str] {
    if mic_render_lname.contains("cable") {
        &["cable output"]
    } else if mic_render_lname.contains("steam streaming microphone") {
        &["steam streaming microphone"]
    } else if mic_render_lname.contains("voicemeeter") {
        &["voicemeeter out", "voicemeeter"]
    } else {
        &["virtual"]
    }
}

/// A render endpoint no loopback should capture: the VB-CABLE (reserved for the mic even when it
/// isn't the chosen target — capturing a cable someone else feeds echoes too), the Steam
/// Streaming Speakers, whose loopback is silent (validated live), and any VoiceMeeter or
/// generically-"virtual" endpoint. VoiceMeeter's strips share one internal mixer: capturing ANY
/// of its render endpoints re-captures what the mic wrote into another one — a digital feedback
/// loop with no acoustic path to break it (mic=`Voicemeeter Input`, loopback=`Voicemeeter Aux
/// Input` used to pass the old name checks). Also the capture-side watchdog's test for "the
/// operator's new default can never work — snap back to the plan".
pub(crate) fn excluded_from_loopback(lname: &str) -> bool {
    lname.contains("cable")
        || lname.contains("steam streaming speakers")
        || lname.contains("voicemeeter")
        || lname.contains("virtual")
}

/// A render endpoint that is SILENT on the host but loopback-capturable — the client-only audio
/// sink. Only the Steam Streaming Microphone's render side qualifies today (validated live).
pub(crate) fn silent_sink(lname: &str) -> bool {
    lname.contains("steam streaming microphone")
}

/// A known-virtual device (cables/streaming endpoints). A render WITHOUT these markers is real
/// hardware — the best loopback source (apps render there by default and the operator can also
/// hear it).
fn virtualish(lname: &str) -> bool {
    lname.contains("virtual")
        || lname.contains("cable")
        || lname.contains("steam streaming")
        || lname.contains("voicemeeter")
}

/// Compute the assignment. `mic_want` is the operator override (`PUNKTFUNK_MIC_DEVICE`,
/// lowercased): when set it beats the built-in candidate order for the mic target. `host_audio`
/// flips the loopback preference to real hardware (audio audible on the host too); the default
/// (`false`) prefers the silent sink so audio plays on the client only. `pad_renders` are the
/// endpoint IDs of the host's own pad-audio ("DualSense speaker") endpoints — platform data
/// collected by `audio_control`, since a pad endpoint is identified by its stamped container /
/// devnode, not by any name rule this module could express.
pub(crate) fn plan(
    renders: &[Endpoint],
    captures: &[Endpoint],
    mic_want: Option<&str>,
    host_audio: bool,
    pad_renders: &[String],
) -> Wiring {
    // 0. Pad-audio endpoints are invisible to the plan: never the mic target (client voice
    //    would play out of a pad "speaker"), never a loopback source (a game's controller
    //    audio cues would stream as desktop audio). Their names carry no virtual marker —
    //    they are stamped "DualSense Wireless Controller" on purpose — so without this
    //    exclusion the loopback rules would read one as real hardware.
    let renders: Vec<Endpoint> = renders
        .iter()
        .filter(|(_, id)| !pad_renders.iter().any(|p| p == id))
        .cloned()
        .collect();
    let renders = renders.as_slice();
    let find_render = |needle: &str| {
        renders
            .iter()
            .find(|(n, _)| n.to_lowercase().contains(needle))
            .cloned()
    };

    // 1. Mic target first — it has the narrower requirements (must be a virtual cable).
    let mic_render = match mic_want {
        Some(w) => find_render(w),
        None => MIC_CANDIDATES.iter().find_map(|c| find_render(c)),
    };

    // 2. Its capture side (what host apps record).
    let mic_capture = mic_render.as_ref().and_then(|(name, _)| {
        capture_for(&name.to_lowercase()).iter().find_map(|c| {
            captures
                .iter()
                .find(|(n, _)| n.to_lowercase().contains(c))
                .cloned()
        })
    });

    // 3. Loopback from the REMAINING renders. Client-only (default): the silent sink (Steam
    //    Streaming Microphone — its loopback works, unlike the Speakers') > real hardware
    //    (audible fallback) > any non-excluded leftover. `host_audio`: real hardware first.
    let not_mic = |id: &str| mic_render.as_ref().is_none_or(|(_, mid)| mid != id);
    let real_hw = || {
        renders.iter().find(|(n, id)| {
            let ln = n.to_lowercase();
            not_mic(id) && !excluded_from_loopback(&ln) && !virtualish(&ln)
        })
    };
    let silent = || {
        renders
            .iter()
            .find(|(n, id)| not_mic(id) && silent_sink(&n.to_lowercase()))
    };
    // `virtualish` here too: a virtual endpoint that slipped past `excluded_from_loopback`'s
    // name list (a future cable/mixer sibling) is an internal-feedback loop waiting to happen —
    // no loopback is the honest answer, exactly like the cable-only case.
    let leftover = || {
        renders.iter().find(|(n, id)| {
            let ln = n.to_lowercase();
            not_mic(id) && !excluded_from_loopback(&ln) && !virtualish(&ln)
        })
    };
    let loopback_render = if host_audio {
        real_hw().or_else(silent).or_else(leftover)
    } else {
        silent().or_else(real_hw).or_else(leftover)
    }
    .cloned();

    Wiring {
        mic_render,
        mic_capture,
        loopback_render,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(name: &str) -> Endpoint {
        (name.to_string(), format!("id-{}", name.to_lowercase()))
    }

    /// The shipped configuration: real output + VB-CABLE (no Steam pair). Mic gets the cable;
    /// with no silent sink the loopback falls back to the speakers (audio audible on both
    /// ends), recording default = CABLE Output.
    #[test]
    fn gaming_pc_with_cable() {
        let renders = [
            ep("Speakers (Realtek HD Audio)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
        ];
        let captures = [
            ep("Microphone (Webcam)"),
            ep("CABLE Output (VB-Audio Virtual Cable)"),
        ];
        let w = plan(&renders, &captures, None, false, &[]);
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
        assert_eq!(
            w.mic_capture.unwrap().0,
            "CABLE Output (VB-Audio Virtual Cable)"
        );
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    /// Client-only (the default): with the full device zoo present — real output, VB-CABLE,
    /// BOTH Steam endpoints — the loopback prefers the silent sink (Steam Streaming
    /// Microphone's render side) over real hardware, so the host speakers stay quiet while
    /// streaming. This is the dissidius/"audio from both PC and phone" configuration.
    #[test]
    fn client_only_prefers_silent_sink_over_hardware() {
        let renders = [
            ep("Speakers (Apple Audio Device)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Speakers)"),
            ep("CABLE In 16ch (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let captures = [
            ep("CABLE Output (VB-Audio Virtual Cable)"),
            ep("Microphone (Steam Streaming Microphone)"),
        ];
        let w = plan(&renders, &captures, None, false, &[]);
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
    }

    /// `PUNKTFUNK_HOST_AUDIO` flips the preference back: real hardware wins the loopback even
    /// when the silent sink exists (the operator wants to hear the stream locally).
    #[test]
    fn host_audio_prefers_real_hardware() {
        let renders = [
            ep("Speakers (Apple Audio Device)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let w = plan(&renders, &[], None, true, &[]);
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Apple Audio Device)"
        );
    }

    /// The multi-render VB-CABLE ("CABLE In 16ch" is a second render endpoint feeding the same
    /// CABLE Output) must never be the loopback in EITHER mode: it feeds the mic's capture side,
    /// and capturing it delivers silence (nothing renders there) — the reported no-audio dud.
    #[test]
    fn cable_16ch_never_loopback() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("CABLE In 16ch (VB-Audio Virtual Cable)"),
        ];
        for host_audio in [false, true] {
            let w = plan(&renders, &[], None, host_audio, &[]);
            assert!(w.loopback_render.is_none(), "host_audio={host_audio}");
        }
    }

    /// THE historical dead-end: headless box where VB-CABLE is the ONLY render endpoint (and
    /// therefore the default). The mic must WIN the cable; the loopback is honestly absent.
    /// (The old anti-echo guard rejected the cable here → mic permanently dead.)
    #[test]
    fn headless_cable_only_mic_wins() {
        let renders = [ep("CABLE Input (VB-Audio Virtual Cable)")];
        let captures = [ep("CABLE Output (VB-Audio Virtual Cable)")];
        let w = plan(&renders, &captures, None, false, &[]);
        assert!(w.mic_render.is_some(), "mic must claim the only cable");
        assert!(w.loopback_render.is_none(), "no echo-safe loopback exists");
    }

    /// Headless with the Steam pair installed: cable = mic, Steam Streaming Microphone = the
    /// loopback (its loopback works; the Speakers' is silent — validated live).
    #[test]
    fn headless_with_steam_pair() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Speakers)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let captures = [
            ep("CABLE Output (VB-Audio Virtual Cable)"),
            ep("Microphone (Steam Streaming Microphone)"),
        ];
        let w = plan(&renders, &captures, None, false, &[]);
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
        assert_eq!(
            w.mic_capture.unwrap().0,
            "CABLE Output (VB-Audio Virtual Cable)"
        );
    }

    /// No cable: the Steam Streaming Microphone doubles as the mic target, and the loopback
    /// must NOT then pick the same endpoint (real hardware wins).
    #[test]
    fn steam_mic_as_target_never_doubles_as_loopback() {
        let renders = [
            ep("Speakers (Steam Streaming Microphone)"),
            ep("Speakers (Realtek HD Audio)"),
        ];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let w = plan(&renders, &captures, None, false, &[]);
        assert_eq!(
            w.mic_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    /// No cable and ONLY the Steam mic: mic wins it, loopback honestly absent (never the same
    /// device — that would echo).
    #[test]
    fn steam_mic_only_no_echo() {
        let renders = [ep("Speakers (Steam Streaming Microphone)")];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let w = plan(&renders, &captures, None, false, &[]);
        assert!(w.mic_render.is_some());
        assert!(w.loopback_render.is_none());
    }

    /// Steam Streaming Speakers never become the loopback (silent loopback, validated live) —
    /// even when they're the only non-mic endpoint.
    #[test]
    fn steam_speakers_never_loopback() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Speakers)"),
        ];
        let w = plan(&renders, &[], None, false, &[]);
        assert!(w.loopback_render.is_none());
    }

    /// Operator override beats the candidate order.
    #[test]
    fn env_override_wins() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Voicemeeter Input (VB-Audio Voicemeeter VAIO)"),
        ];
        let captures = [ep("Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)")];
        let w = plan(&renders, &captures, Some("voicemeeter input"), false, &[]);
        assert_eq!(
            w.mic_render.unwrap().0,
            "Voicemeeter Input (VB-Audio Voicemeeter VAIO)"
        );
        assert_eq!(
            w.mic_capture.unwrap().0,
            "Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)"
        );
    }

    /// No virtual device anywhere: no mic target (open fails with guidance), loopback = the
    /// real output — desktop audio unaffected.
    #[test]
    fn no_virtual_device() {
        let renders = [ep("Speakers (Realtek HD Audio)")];
        let w = plan(&renders, &[], None, false, &[]);
        assert!(w.mic_render.is_none());
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    /// VoiceMeeter box with real hardware: no cable, so the mic takes `Voicemeeter Input` — and
    /// the loopback must NEVER be `Voicemeeter Aux Input` (same internal mixer: capturing any
    /// VoiceMeeter render re-captures what the mic wrote — a digital feedback loop). Real
    /// hardware wins the loopback in both modes.
    #[test]
    fn voicemeeter_aux_never_pairs_with_voicemeeter_mic() {
        let renders = [
            ep("Voicemeeter Input (VB-Audio Voicemeeter VAIO)"),
            ep("Voicemeeter Aux Input (VB-Audio Voicemeeter AUX VAIO)"),
            ep("Speakers (Realtek HD Audio)"),
        ];
        let captures = [ep("Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)")];
        for host_audio in [false, true] {
            let w = plan(&renders, &captures, None, host_audio, &[]);
            assert_eq!(
                w.mic_render.as_ref().unwrap().0,
                "Voicemeeter Input (VB-Audio Voicemeeter VAIO)",
                "host_audio={host_audio}"
            );
            assert_eq!(
                w.loopback_render.as_ref().unwrap().0,
                "Speakers (Realtek HD Audio)",
                "host_audio={host_audio}"
            );
        }
    }

    /// Only VoiceMeeter endpoints (headless mixer box): mic wins one, the loopback is honestly
    /// absent — like the cable-only case, never another strip of the same mixer.
    #[test]
    fn voicemeeter_only_no_loopback() {
        let renders = [
            ep("Voicemeeter Input (VB-Audio Voicemeeter VAIO)"),
            ep("Voicemeeter Aux Input (VB-Audio Voicemeeter AUX VAIO)"),
        ];
        for host_audio in [false, true] {
            let w = plan(&renders, &[], None, host_audio, &[]);
            assert!(w.mic_render.is_some(), "host_audio={host_audio}");
            assert!(w.loopback_render.is_none(), "host_audio={host_audio}");
        }
    }

    /// A generically-"virtual" leftover (unknown vendor cable) is refused too: `leftover()`
    /// applies `virtualish`, so a virtual endpoint that slips past the name list can't become
    /// the loopback.
    #[test]
    fn unknown_virtual_never_loopback() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Some Virtual Audio Device)"),
        ];
        let w = plan(&renders, &[], None, false, &[]);
        assert!(w.loopback_render.is_none());
    }

    /// A provisioned pad-audio endpoint (stamped "DualSense Wireless Controller") is invisible
    /// to the plan. Its name carries NO virtual marker — on purpose, games must read it as the
    /// pad's speaker — so the name rules alone would classify it as real hardware and hand it
    /// the loopback; only the id exclusion prevents that. Measured fact: the wiring plan on the
    /// target box already enumerated a stamped endpoint among `renders`.
    #[test]
    fn pad_endpoints_invisible() {
        let renders = [
            ep("DualSense Wireless Controller"),
            ep("Speakers (Realtek HD Audio)"),
        ];
        let pads = [renders[0].1.clone()];
        let w = plan(&renders, &[], None, false, &pads);
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
        // Even an operator mic override matching the pad's name must not claim it; with the
        // pad as the only render endpoint there is honestly no mic target and no loopback.
        let w = plan(
            &renders[..1],
            &[],
            Some("wireless controller"),
            false,
            &pads,
        );
        assert!(w.mic_render.is_none());
        assert!(w.loopback_render.is_none());
    }
}
