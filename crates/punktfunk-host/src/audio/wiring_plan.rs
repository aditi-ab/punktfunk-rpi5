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
//! client's own audio stream. **Tier-0** avoids the collision by construction: the host mints
//! its OWN pair from Steam's streaming drivers ([`MintedIds`] — "Punktfunk Microphone" for the
//! mic, "Punktfunk Speakers" for the loopback, matched by ID because their names are identical
//! to Steam's primaries). Below tier-0, the name ladder keeps the old discipline: the mic is
//! assigned FIRST (VB-CABLE was bundled by installers until the audio-substrate change; a
//! user-installed cable still serves) and the loopback gets a *different* endpoint; when only
//! the cable exists (headless box, no other output), the MIC wins and the loopback is honestly
//! unavailable. The old code did the opposite — the mic refused the cable because it was the
//! default render endpoint — which permanently killed mic passthrough on exactly that box.
//!
//! **One exception to mic-first — game audio outranks the mic.** The Steam Streaming
//! Microphone's render side is ALSO the only silent client-only loopback sink, so the mic may
//! take it only while the loopback still gets a preferred (non-last-resort) pick without it:
//! another silent sink, or real hardware. When taking it would leave desktop audio on the
//! known-silent Speakers or on nothing — the cable-less headless box, the recurring field
//! failure — the loopback gets the endpoint and the mic falls to a lesser candidate or is
//! honestly unavailable ([`Wiring::mic_withheld`]), with guidance naming the trade. The
//! cable-only rule above is untouched (a cable can never be a loopback, so the mic still wins
//! it), and an operator `PUNKTFUNK_MIC_DEVICE` override also still wins — an explicit choice
//! beats the trade-off.
//!
//! **Loopback preference depends on where the audio should be heard.** The default is
//! *client-only*: prefer a render endpoint that is silent on the host but has a WORKING loopback
//! (the Steam Streaming *Microphone*'s render side — validated live; the Steam Streaming
//! *Speakers*' loopback is silent) so the desktop mix reaches the stream without also blasting
//! out of the host's speakers. Real hardware is the fallback (audio then plays on both ends).
//! With `host_audio` (the `PUNKTFUNK_HOST_AUDIO` opt-in) the order flips back: real hardware
//! first, so the operator hears the stream locally.
//!
//! **Last resort, and the honest failure.** When neither a silent sink nor real hardware
//! survives the mic reservation, the Steam Streaming *Speakers* are taken as a flagged LAST
//! resort ([`Wiring::loopback_last_resort`]): their loopback is known-silent (validated live) —
//! a QUALITY risk the capture side warns about and treats as a stopgap — but holding a parked
//! endpoint beats holding none (2026-08 field case: the display isolate invalidated the only
//! real render endpoint mid-session, the mic held the Streaming Microphone, and a plan with no
//! loopback left the session unrecoverable). Cables, VoiceMeeter strips and generically-
//! "virtual" endpoints are never a last resort — capturing them re-captures what the mic writes,
//! an echo/feedback CORRECTNESS risk, unlike silence — so with only those left the plan is
//! honestly unsatisfiable ([`Wiring::loopback_unsatisfiable`]): a pure verdict on the endpoint
//! set that cannot change until the set does. Callers must wait for an endpoint-set change
//! ([`fingerprint`]), not retry.

/// A `(friendly_name, endpoint_id)` pair as enumerated from WASAPI.
pub(crate) type Endpoint = (String, String);

/// A render endpoint's ENGINE MIX FORMAT, as `IAudioClient::GetMixFormat` reports it.
///
/// This is the number the 2026-08-03 field report needed and the log did not have. The capture
/// side opens with `autoconvert: true` and asks for 48 kHz f32 in the wire layout, so WASAPI
/// silently converts whatever the endpoint really runs — and the "48 kHz f32 channels=2" we
/// logged was our REQUEST, not the source. An endpoint that mixes at 24 kHz mono therefore
/// produced a 48 kHz stereo stream that had already been through a 24 kHz mono bottleneck, with
/// nothing in any log to say so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MixFormat {
    pub rate_hz: u32,
    pub channels: u16,
    pub bits: u16,
}

impl MixFormat {
    /// Why this endpoint would NARROW a `want`-channel desktop mix, or `None` if it carries it
    /// intact. Bit depth is deliberately not a criterion: 16-bit is ~96 dB of headroom, far below
    /// Opus's own noise floor, whereas a lost channel or halved bandwidth is plainly audible.
    pub(crate) fn narrowing(&self, want: u8) -> Option<String> {
        if self.rate_hz < 48_000 && self.channels < want as u16 {
            return Some(format!(
                "mixes at {} Hz and only {} channel(s)",
                self.rate_hz, self.channels
            ));
        }
        if self.rate_hz < 48_000 {
            return Some(format!(
                "mixes at {} Hz, so the stream is band-limited to ~{} kHz before Opus sees it",
                self.rate_hz,
                self.rate_hz / 2000
            ));
        }
        if self.channels < want as u16 {
            return Some(format!(
                "mixes {} channel(s), so a {want}-channel desktop mix is downmixed and re-expanded",
                self.channels
            ));
        }
        None
    }
}

/// Looks up a render endpoint's mix format by endpoint id. `None` = unknown (enumeration failed,
/// or the caller has no way to ask) — treated as "assume it is fine", so a probe failure can
/// never make the plan worse than it was before formats existed.
pub(crate) type FormatProbe<'a> = &'a dyn Fn(&Endpoint) -> Option<MixFormat>;

/// A [`FormatProbe`] that knows nothing — the pre-WP2.1 behaviour.
pub(crate) fn no_formats(_: &Endpoint) -> Option<MixFormat> {
    None
}

/// The host's own MINTED endpoints — instances of Valve's streaming-audio driver the
/// [`minted`](super::minted) provider created at startup — by WASAPI endpoint id.
///
/// Tier-0 is an IDENTITY tier, not a name tier: a minted instance is indistinguishable by
/// friendly name from Steam's own primaries (S1 measured exactly that confusion — the probe's
/// name match grabbed a stamped instance instead of the primary), so the provider records what
/// it minted and the plan matches by id. All fields empty when nothing is minted (Steam
/// absent, provisioning disabled or still running) — every rule then falls back to the
/// name-based ladder unchanged.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct MintedIds {
    /// "Punktfunk Speakers" — an SSS-driver instance reserved as the client-only loopback
    /// sink. Never contended by Steam's own Remote Play, deterministic across re-plans.
    pub speakers_render: Option<String>,
    /// "Punktfunk Microphone" render side — the virtual mic's write target.
    pub mic_render: Option<String>,
    /// "Punktfunk Microphone" capture side — the microphone host apps record.
    pub mic_capture: Option<String>,
}

/// The one-line runtime answer "does desktop audio work, does the mic work" — the §C4
/// classification (logged with every plan change; the status API surfaces it later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioReadiness {
    /// Both roles have endpoints.
    Full,
    /// Desktop audio yes, mic passthrough no.
    AudioOnly,
    /// Mic yes, desktop audio no.
    MicOnly,
    /// Neither role has an endpoint.
    Nothing,
}

pub(crate) fn readiness(w: &Wiring) -> AudioReadiness {
    match (w.loopback_render.is_some(), w.mic_render.is_some()) {
        (true, true) => AudioReadiness::Full,
        (true, false) => AudioReadiness::AudioOnly,
        (false, true) => AudioReadiness::MicOnly,
        (false, false) => AudioReadiness::Nothing,
    }
}

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
    /// `loopback_render` is the flagged LAST RESORT (the Steam Streaming Speakers, whose
    /// loopback is known-silent — validated live), taken only because nothing better survived
    /// the mic reservation. The capture side treats it as a stopgap: it warns when the silence
    /// materializes and re-plans on any endpoint-set change instead of riding it out.
    pub loopback_last_resort: bool,
    /// Set when the chosen loopback endpoint's mix format NARROWS the desktop mix (see
    /// [`MixFormat::narrowing`]) and the plan took it anyway because nothing better existed. Carries
    /// the human-readable reason for the capture side to log — a quality risk the operator can act
    /// on (attach a real output, or set the output mode to prefer hardware), not a failure.
    pub loopback_narrowing: Option<String>,
    /// The mic was DENIED the Steam Streaming Microphone because taking it would have left the
    /// loopback with only the known-silent last resort or nothing — game audio outranks the
    /// optional mic. (`mic_render` may still hold a lesser candidate; when it is `None` the
    /// mic open fails with guidance naming the trade — a cable gives the mic its own device
    /// without costing the loopback.)
    pub mic_withheld: bool,
}

impl Wiring {
    /// This plan has NO loopback endpoint — not even the last resort. Because [`plan`] is pure,
    /// this is a STRUCTURAL verdict on the endpoint set, not a transient device error:
    /// reattempting a capture open without an endpoint-set change must fail identically (the
    /// 2026-08 field case spent 8+ minutes of flat 2 s retries proving exactly that). Callers
    /// wait for the set's [`fingerprint`] to move instead of retrying.
    pub(crate) fn loopback_unsatisfiable(&self) -> bool {
        self.loopback_render.is_none()
    }
}

/// Render-endpoint friendly-name substrings (lowercased) usable as the virtual-mic write target,
/// ordered by preference — the NAME ladder below the minted tier-0 ([`MintedIds`] outranks all
/// of these). VB-CABLE first among the names: installers bundled it for the mic until the
/// audio-substrate change, and a user-installed cable still serves.
const MIC_CANDIDATES: &[&str] = &[
    "cable input", // VB-Audio Virtual Cable — user-installed / from older bundled installs
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

/// A capture endpoint that surfaces a VIRTUAL device's audio (cables, streaming mics, mixer
/// strips, the host's own minted "Punktfunk" microphone) rather than a real microphone. The
/// recording-default hygiene pass must never move the box's default onto one of these.
pub(crate) fn virtual_capture(lname: &str) -> bool {
    lname.contains("cable output")
        || lname.contains("steam streaming")
        || lname.contains("voicemeeter")
        || lname.contains("virtual")
        || lname.contains("punktfunk")
}

/// The first REAL capture endpoint (skipping `avoid_id` and every [`virtual_capture`]) — where
/// the recording-default hygiene sends a default an earlier build left parked on the virtual mic
/// while the host is idle. `None` on a box with no real microphone: nothing sane to move to, so
/// the default is left alone.
pub(crate) fn real_capture<'a>(
    captures: &'a [Endpoint],
    avoid_id: Option<&str>,
) -> Option<&'a Endpoint> {
    captures
        .iter()
        .find(|(n, id)| Some(id.as_str()) != avoid_id && !virtual_capture(&n.to_lowercase()))
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

/// Is this render endpoint id one of the virtual pad's audio endpoints?
///
/// Pulled out of [`plan`] because the plan is NOT the only place that must not treat these as
/// ordinary hardware — see [`excluded_from_loopback`]'s callers. A pad endpoint is deliberately
/// stamped with the controller's own name ("DualSense Wireless Controller") so games read it as
/// the pad's speaker, which means no name-based rule can recognise one; the only reliable test is
/// identity against the ids the pad-endpoint provisioner created.
pub(crate) fn is_pad_render(id: &str, pad_renders: &[String]) -> bool {
    pad_renders.iter().any(|p| p == id)
}

/// Compute the assignment. `mic_want` is the operator override (`PUNKTFUNK_MIC_DEVICE`,
/// lowercased): when set it beats the built-in candidate order for the mic target. `host_audio`
/// flips the loopback preference to real hardware (audio audible on the host too); the default
/// (`false`) prefers the silent sink so audio plays on the client only.
pub(crate) fn plan(
    renders: &[Endpoint],
    captures: &[Endpoint],
    mic_want: Option<&str>,
    host_audio: bool,
    pad_renders: &[String],
    minted: &MintedIds,
) -> Wiring {
    plan_with_formats(
        renders,
        captures,
        mic_want,
        host_audio,
        &no_formats,
        2,
        pad_renders,
        minted,
    )
}

/// [`plan`] with knowledge of each render endpoint's engine mix format, and the channel count the
/// session wants to carry.
///
/// **The 2026-08-03 field report is this function's reason to exist.** The default client-only
/// preference takes the "silent sink" — Steam's Streaming *Microphone* render endpoint — over real
/// hardware unconditionally, because it is silent on the host. But that endpoint exists to carry
/// remote *voice*, and nothing checked whether it could carry music. On the reporter's box it won
/// all 31 loopback opens across 25 sessions while a clean AMD HD Audio endpoint sat idle, and the
/// whole desktop mix went through it before reaching Opus.
///
/// So a silent sink now has to EARN its preference: if its mix format narrows the mix (see
/// [`MixFormat::narrowing`]) it drops below real hardware. It is still taken when nothing better
/// exists — narrow audio beats no audio — but flagged in [`Wiring::loopback_narrowing`] so the
/// capture side can say why. An unknown format (probe failed) counts as fine, so this can never
/// make the plan worse than it was before formats existed.
#[allow(clippy::too_many_arguments)] // mirrors the enumeration inputs; a param struct would only rename the problem
pub(crate) fn plan_with_formats(
    renders: &[Endpoint],
    captures: &[Endpoint],
    mic_want: Option<&str>,
    host_audio: bool,
    format_of: FormatProbe,
    want_channels: u8,
    pad_renders: &[String],
    minted: &MintedIds,
) -> Wiring {
    // 0. Pad-audio endpoints are invisible to the plan: never the mic target (client voice
    //    would play out of a pad "speaker"), never a loopback source (a game's controller
    //    audio cues would stream as desktop audio), and — since this shadows `renders` for
    //    every tier below — never the flagged last resort either. Their names carry no virtual
    //    marker (they are stamped "DualSense Wireless Controller" on purpose, so games read
    //    them as the pad's speaker), so the name rules alone would take one for real hardware.
    let renders: Vec<Endpoint> = renders
        .iter()
        .filter(|(_, id)| !is_pad_render(id, pad_renders))
        .cloned()
        .collect();
    let renders = renders.as_slice();
    let find_render = |needle: &str| {
        renders
            .iter()
            .find(|(n, _)| n.to_lowercase().contains(needle))
            .cloned()
    };

    // Tier-0 lookups: the minted ids resolved against THIS enumeration (an id the provider
    // recorded but audiosrv no longer serves must not produce a phantom assignment).
    let find_by_id = |id: &Option<String>| -> Option<Endpoint> {
        id.as_deref()
            .and_then(|id| renders.iter().find(|(_, rid)| rid == id).cloned())
    };
    let minted_mic = find_by_id(&minted.mic_render);
    let minted_sink = find_by_id(&minted.speakers_render);

    // 1. Mic target first — it has the narrower requirements (must be a virtual cable). The
    //    minted "Punktfunk Microphone" outranks every name-based candidate: it exists for
    //    exactly this role, and taking it can never cost the loopback anything (the minted
    //    sink is its counterpart). An operator override still beats it.
    let mic_render = match mic_want {
        Some(w) => find_render(w),
        None => minted_mic
            .clone()
            .or_else(|| MIC_CANDIDATES.iter().find_map(|c| find_render(c))),
    };
    // Game audio outranks the mic: the Steam Streaming Microphone's render side is also the
    // only silent client-only loopback sink, so the mic may hold it only while the loopback
    // still gets a PREFERRED (non-last-resort) pick without it — another silent sink or real
    // hardware, the same two tiers both preference orders draw from. Otherwise the endpoint
    // goes to the loopback and the mic falls to a lesser candidate or (honestly) to none.
    // Before this rule, the cable-less headless Steam box streamed SILENCE: the mic held the
    // Streaming Microphone and the loopback got the known-silent Speakers (the 2026-08 field
    // case). An operator override is exempt — an explicit PUNKTFUNK_MIC_DEVICE beats the
    // trade-off.
    let mut mic_withheld = false;
    let mic_render = match mic_render {
        Some((name, id)) if mic_want.is_none() && silent_sink(&name.to_lowercase()) => {
            let loopback_survives = renders.iter().any(|(n, rid)| {
                let ln = n.to_lowercase();
                *rid != id
                    && (silent_sink(&ln) || (!excluded_from_loopback(&ln) && !virtualish(&ln)))
            });
            if loopback_survives {
                Some((name, id))
            } else {
                mic_withheld = true;
                // Skip the silent-sink candidate; a lesser candidate may still serve the mic.
                MIC_CANDIDATES
                    .iter()
                    .filter(|c| !silent_sink(c))
                    .find_map(|c| find_render(c))
            }
        }
        other => other,
    };

    // 2. Its capture side (what host apps record). A minted mic resolves by the provider's
    //    recorded CAPTURE id — a name search cannot tell the minted microphone from Steam's
    //    primary (same friendly name), and pairing the minted render with the primary's
    //    capture would record a mic nothing writes into.
    let mic_capture = mic_render.as_ref().and_then(|(name, id)| {
        if Some(id) == minted.mic_render.as_ref() {
            return minted
                .mic_capture
                .as_deref()
                .and_then(|cid| captures.iter().find(|(_, c)| c == cid).cloned());
        }
        capture_for(&name.to_lowercase()).iter().find_map(|c| {
            captures
                .iter()
                .find(|(n, _)| n.to_lowercase().contains(c))
                .cloned()
        })
    });

    // 3. Loopback from the REMAINING renders. Client-only (default): the silent sink (Steam
    //    Streaming Microphone — its loopback works, unlike the Speakers') > real hardware
    //    (audible fallback). `host_audio`: real hardware first. Either order can fall through
    //    to the flagged last resort below.
    let not_mic = |id: &str| mic_render.as_ref().is_none_or(|(_, mid)| mid != id);
    let real_hw = || {
        renders.iter().find(|(n, id)| {
            let ln = n.to_lowercase();
            not_mic(id) && !excluded_from_loopback(&ln) && !virtualish(&ln)
        })
    };
    // A silent sink splits in two: one that carries the mix intact, and one that narrows it. The
    // first keeps the historical preference; the second falls BELOW real hardware.
    let narrowing_of = |ep: &Endpoint| format_of(ep).and_then(|f| f.narrowing(want_channels));
    let silent_intact = || {
        renders.iter().find(|ep| {
            not_mic(&ep.1) && silent_sink(&ep.0.to_lowercase()) && narrowing_of(ep).is_none()
        })
    };
    let silent_narrow = || {
        renders.iter().find(|ep| {
            not_mic(&ep.1) && silent_sink(&ep.0.to_lowercase()) && narrowing_of(ep).is_some()
        })
    };
    // LAST RESORT — the Steam Streaming Speakers, and ONLY them. Their loopback is known-silent
    // (validated live): a QUALITY risk, flagged so the capture side can warn when the silence
    // materializes and re-plan when the endpoint set changes — but a parked endpoint beats none
    // (2026-08: the display isolate invalidated the only real render endpoint mid-session and a
    // loopback-less plan left the session unrecoverable). Never a cable, a VoiceMeeter strip, or
    // a generically-"virtual" endpoint: those re-capture what the mic writes — echo/feedback
    // CORRECTNESS risks — so "no loopback" stays the honest answer there. NOTE
    // `excluded_from_loopback` itself stays untouched: it also powers the capture watchdog's
    // judgement of a NEW operator-chosen default, where admitting the Speakers would change
    // mid-stream snap-back semantics.
    let last_resort = || {
        renders
            .iter()
            .find(|(n, id)| not_mic(id) && n.to_lowercase().contains("steam streaming speakers"))
    };
    // Tier-0 sink: the minted "Punktfunk Speakers". Same quality discipline as every silent
    // sink — a narrowing minted instance demotes below real hardware rather than silently
    // costing quality (S2 measured the driver clean at 48 kHz stereo, so this is a guard, not
    // an expectation).
    let minted_intact = || {
        minted_sink
            .as_ref()
            .filter(|(_, id)| not_mic(id))
            .filter(|ep| narrowing_of(ep).is_none())
    };
    let minted_narrow = || {
        minted_sink
            .as_ref()
            .filter(|(_, id)| not_mic(id))
            .filter(|ep| narrowing_of(ep).is_some())
    };
    // A narrowing silent sink sits below real hardware in BOTH modes: preferring silence on the
    // host is a routing choice, but it must not silently cost audio quality when a clean endpoint
    // is right there. The minted sink heads its tier in both modes — it is the one endpoint
    // whose whole purpose is this role.
    let preferred = if host_audio {
        real_hw()
            .or_else(minted_intact)
            .or_else(silent_intact)
            .or_else(minted_narrow)
            .or_else(silent_narrow)
    } else {
        minted_intact()
            .or_else(silent_intact)
            .or_else(real_hw)
            .or_else(minted_narrow)
            .or_else(silent_narrow)
    };
    let (loopback_render, loopback_last_resort) = match preferred {
        Some(ep) => (Some(ep.clone()), false),
        None => match last_resort() {
            Some(ep) => (Some(ep.clone()), true),
            None => (None, false),
        },
    };
    // Report narrowing for whatever we actually chose — including real hardware, which can also
    // be a 24 kHz mono endpoint (a headset's hands-free profile is exactly that).
    let loopback_narrowing = loopback_render.as_ref().and_then(narrowing_of);

    Wiring {
        mic_render,
        mic_capture,
        loopback_render,
        loopback_last_resort,
        loopback_narrowing,
        mic_withheld,
    }
}

/// Order-independent fingerprint of an enumerated endpoint set. [`plan`] is a pure function of
/// these inputs (the env knobs are process-stable), so an unchanged fingerprint PROVES an
/// unchanged verdict: re-planning an unsatisfiable set before the fingerprint moves only repeats
/// the same answer, with IPolicyConfig default-device writes as the side effect. The capture
/// loop polls this instead of re-planning, and treats a change as the recovery moment.
pub(crate) fn fingerprint(renders: &[Endpoint], captures: &[Endpoint]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    // Each direction hashes as a length-prefixed sorted slice, so renders and captures cannot
    // alias each other and endpoint order (enumeration order churns) never matters.
    for eps in [renders, captures] {
        let mut sorted: Vec<&Endpoint> = eps.iter().collect();
        sorted.sort();
        sorted.hash(&mut h);
    }
    h.finish()
}

/// The one-shot diagnosis for a plan with no loopback endpoint: every enumerated render with WHY
/// it was rejected, then ONLY the remedies not already taken. The static advice this replaces
/// ("attach one, or let the host install the Steam Streaming pair") was already satisfied in the
/// 2026-08 field case — the pair WAS installed, its Microphone half reserved by the mic — so the
/// message pointed at a fix the box already had. Pure, like [`plan`]: callers pass the same
/// enumeration the plan consumed.
pub(crate) fn describe_no_loopback(renders: &[Endpoint], wiring: &Wiring) -> String {
    debug_assert!(wiring.loopback_unsatisfiable());
    let mic_id = wiring.mic_render.as_ref().map(|(_, id)| id.as_str());
    let rejected: Vec<String> = renders
        .iter()
        .map(|(name, id)| {
            let ln = name.to_lowercase();
            let why = if Some(id.as_str()) == mic_id {
                "reserved for the virtual mic (its loopback would echo the client's voice back)"
            } else if ln.contains("cable") {
                "virtual cable (its loopback re-captures what is written into it)"
            } else if ln.contains("voicemeeter") {
                "VoiceMeeter strip (shares the mixer the mic writes into — a feedback loop)"
            } else if ln.contains("steam streaming speakers") {
                // Reachable only when the Speakers ARE the mic target (operator override) —
                // the last-resort tier takes them otherwise.
                "known-silent loopback (validated live)"
            } else if ln.contains("virtual") {
                "unrecognized virtual endpoint (assumed feedback/silence risk)"
            } else {
                // `plan` accepts any non-virtual render — reaching this arm means a tier
                // changed without updating this diagnosis.
                "rejected by the wiring plan"
            };
            format!("{name:?}: {why}")
        })
        .collect();
    let inventory = if rejected.is_empty() {
        "no render endpoints exist at all".to_string()
    } else {
        rejected.join("; ")
    };
    let has = |needle: &str| {
        renders
            .iter()
            .any(|(n, _)| n.to_lowercase().contains(needle))
    };
    let mut remedies = vec!["attach any output device (headphones, or a monitor/TV with audio)"];
    // Only useful when the mic would actually vacate a loopback-capable endpoint: with the mic
    // on the Steam Streaming Microphone, a cable frees that silent sink for the loopback. A mic
    // on a VoiceMeeter strip frees nothing capturable, so the advice is withheld there.
    if !has("cable")
        && wiring
            .mic_render
            .as_ref()
            .is_some_and(|(n, _)| silent_sink(&n.to_lowercase()))
    {
        remedies.push(
            "install VB-Audio Virtual Cable — the mic then takes the cable and frees the Steam \
             Streaming Microphone's render side for the loopback",
        );
    }
    if !has("steam streaming microphone") {
        remedies.push(
            "install Steam — its Remote Play streaming drivers add a loopback-capable virtual \
             sink",
        );
    }
    format!(
        "no loopback-capturable render endpoint: {inventory}. Remedies: {}",
        remedies.join("; or ")
    )
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
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
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
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
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
        let w = plan(&renders, &[], None, true, &[], &MintedIds::default());
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
            let w = plan(&renders, &[], None, host_audio, &[], &MintedIds::default());
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
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
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
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
        assert!(
            !w.loopback_last_resort,
            "the silent sink is a PREFERRED pick"
        );
        assert_eq!(
            w.mic_capture.unwrap().0,
            "CABLE Output (VB-Audio Virtual Cable)"
        );
    }

    /// No cable: the Steam Streaming Microphone doubles as the mic target — allowed, because
    /// the loopback still gets real hardware — and the loopback must NOT then pick the same
    /// endpoint.
    #[test]
    fn steam_mic_as_target_never_doubles_as_loopback() {
        let renders = [
            ep("Speakers (Steam Streaming Microphone)"),
            ep("Speakers (Realtek HD Audio)"),
        ];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(
            w.mic_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
        assert!(!w.mic_withheld);
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    /// No cable and ONLY the Steam mic: GAME AUDIO wins the endpoint — the loopback takes the
    /// render side (a working silent sink) and the mic is honestly withheld. The old rule gave
    /// the mic the endpoint and the stream was silent.
    #[test]
    fn steam_mic_only_audio_wins() {
        let renders = [ep("Speakers (Steam Streaming Microphone)")];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert!(w.mic_render.is_none());
        assert!(w.mic_withheld);
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
        assert!(!w.loopback_last_resort);
    }

    /// Cable absent but a VoiceMeeter strip exists: the withheld mic falls to the lesser
    /// candidate instead of dying — mic on the strip, loopback on the freed Streaming
    /// Microphone render side. Both features work without a cable.
    #[test]
    fn withheld_mic_falls_to_voicemeeter() {
        let renders = [
            ep("Speakers (Steam Streaming Speakers)"),
            ep("Speakers (Steam Streaming Microphone)"),
            ep("Voicemeeter Input (VB-Audio Voicemeeter VAIO)"),
        ];
        let captures = [
            ep("Microphone (Steam Streaming Microphone)"),
            ep("Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)"),
        ];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(
            w.mic_render.as_ref().unwrap().0,
            "Voicemeeter Input (VB-Audio Voicemeeter VAIO)"
        );
        assert!(w.mic_withheld);
        assert_eq!(
            w.mic_capture.unwrap().0,
            "Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)"
        );
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
        assert!(!w.loopback_last_resort);
    }

    /// Steam Streaming Speakers are never a PREFERRED loopback (their loopback is silent —
    /// validated live) — but when they are the only non-mic endpoint they ARE taken, flagged as
    /// the last resort: a silent loopback the capture side can warn about beats a plan with no
    /// endpoint at all (which is unrecoverable until the topology changes).
    #[test]
    fn steam_speakers_only_as_last_resort() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Speakers)"),
        ];
        for host_audio in [false, true] {
            let w = plan(&renders, &[], None, host_audio, &[], &MintedIds::default());
            assert_eq!(
                w.loopback_render.as_ref().unwrap().0,
                "Speakers (Steam Streaming Speakers)",
                "host_audio={host_audio}"
            );
            assert!(w.loopback_last_resort, "host_audio={host_audio}");
        }
    }

    /// THE 2026-08 field case, re-decided: no cable, only the Steam pair left after the display
    /// isolate invalidated the monitor's DP audio endpoint. Game audio now OUTRANKS the mic —
    /// the loopback takes the Streaming Microphone's render side (a WORKING silent sink)
    /// instead of the mic holding it and stranding the loopback on the known-silent Speakers.
    /// Audio streams; the mic is honestly withheld. Holds in both preference modes.
    #[test]
    fn field_case_steam_pair_only_audio_outranks_mic() {
        let renders = [
            ep("Altavoces (Steam Streaming Speakers)"),
            ep("Altavoces (Steam Streaming Microphone)"),
        ];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        for host_audio in [false, true] {
            let w = plan(
                &renders,
                &captures,
                None,
                host_audio,
                &[],
                &MintedIds::default(),
            );
            assert!(w.mic_render.is_none(), "host_audio={host_audio}");
            assert!(w.mic_withheld, "host_audio={host_audio}");
            assert_eq!(
                w.loopback_render.as_ref().unwrap().0,
                "Altavoces (Steam Streaming Microphone)",
                "host_audio={host_audio}"
            );
            assert!(!w.loopback_last_resort, "host_audio={host_audio}");
        }
    }

    /// The operator override is exempt from game-audio-outranks-the-mic: pinning the mic to
    /// the Streaming Microphone strands the loopback on the last resort, and that is the
    /// operator's explicit call.
    #[test]
    fn env_override_may_strand_the_loopback() {
        let renders = [
            ep("Altavoces (Steam Streaming Speakers)"),
            ep("Altavoces (Steam Streaming Microphone)"),
        ];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let w = plan(
            &renders,
            &captures,
            Some("steam streaming microphone"),
            false,
            &[],
            &MintedIds::default(),
        );
        assert_eq!(
            w.mic_render.unwrap().0,
            "Altavoces (Steam Streaming Microphone)"
        );
        assert!(!w.mic_withheld);
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Altavoces (Steam Streaming Speakers)"
        );
        assert!(w.loopback_last_resort);
    }

    /// The last resort never shadows a real pick: with real hardware present the Speakers stay
    /// unchosen and the flag stays down, in both preference modes.
    #[test]
    fn last_resort_never_beats_real_hardware() {
        let renders = [
            ep("Speakers (Steam Streaming Microphone)"),
            ep("Speakers (Steam Streaming Speakers)"),
            ep("Speakers (Realtek HD Audio)"),
        ];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        for host_audio in [false, true] {
            let w = plan(
                &renders,
                &captures,
                None,
                host_audio,
                &[],
                &MintedIds::default(),
            );
            assert_eq!(
                w.loopback_render.as_ref().unwrap().0,
                "Speakers (Realtek HD Audio)",
                "host_audio={host_audio}"
            );
            assert!(!w.loopback_last_resort, "host_audio={host_audio}");
        }
    }

    /// Cables and VoiceMeeter strips are CORRECTNESS risks (they re-capture what the mic
    /// writes — echo/feedback), not quality risks: never the loopback, not even as a last
    /// resort. The plan stays honestly unsatisfiable.
    #[test]
    fn cable_and_voicemeeter_never_last_resort() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("CABLE In 16ch (VB-Audio Virtual Cable)"),
            ep("Voicemeeter Aux Input (VB-Audio Voicemeeter AUX VAIO)"),
        ];
        let captures = [ep("CABLE Output (VB-Audio Virtual Cable)")];
        for host_audio in [false, true] {
            let w = plan(
                &renders,
                &captures,
                None,
                host_audio,
                &[],
                &MintedIds::default(),
            );
            assert!(w.loopback_render.is_none(), "host_audio={host_audio}");
            assert!(!w.loopback_last_resort, "host_audio={host_audio}");
            assert!(w.loopback_unsatisfiable(), "host_audio={host_audio}");
        }
    }

    // ---- format-aware loopback selection (WP2.1) -----------------------------------------

    fn fmt(rate_hz: u32, channels: u16) -> MixFormat {
        MixFormat {
            rate_hz,
            channels,
            bits: 32,
        }
    }

    /// Probe helper: give endpoints whose (lowercased) name contains a needle that format,
    /// everything else unknown. Owns its table so call sites can pass a literal inline.
    fn probe(table: Vec<(&'static str, MixFormat)>) -> impl Fn(&Endpoint) -> Option<MixFormat> {
        move |ep: &Endpoint| {
            let name = ep.0.to_lowercase();
            table
                .iter()
                .find_map(|(needle, f)| name.contains(needle).then_some(*f))
        }
    }

    /// THE 2026-08-03 field case, with formats. The reporter's exact endpoint inventory: the plan
    /// took the Steam Streaming Microphone on all 31 opens while a clean AMD HD Audio endpoint sat
    /// idle. Once we can see that the silent sink narrows the mix, real hardware must win.
    #[test]
    fn narrowing_silent_sink_loses_to_real_hardware() {
        let renders = [
            ep("CABLE In 16ch (VB-Audio Virtual Cable)"),
            ep("Altavoces (Steam Streaming Speakers)"),
            ep("Altavoces (Steam Streaming Microphone)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("1 - Odyssey G60SD (AMD High Definition Audio Device)"),
        ];
        let captures = [
            ep("CABLE Output (VB-Audio Virtual Cable)"),
            ep("Microphone (Steam Streaming Microphone)"),
        ];
        // A voice-carrier endpoint: 24 kHz mono.
        let p = probe(vec![
            ("steam streaming microphone", fmt(24_000, 1)),
            ("odyssey", fmt(48_000, 2)),
        ]);
        let w = plan_with_formats(
            &renders,
            &captures,
            None,
            false,
            &p,
            2,
            &[],
            &MintedIds::default(),
        );
        assert_eq!(
            w.loopback_render.as_ref().unwrap().0,
            "1 - Odyssey G60SD (AMD High Definition Audio Device)",
            "a narrowing silent sink must not beat clean real hardware"
        );
        assert!(
            w.loopback_narrowing.is_none(),
            "the chosen endpoint is intact"
        );
        // The mic assignment is untouched by any of this.
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
    }

    /// …but a silent sink that carries the mix intact keeps its historical preference: the
    /// client-only routing default is not being abandoned, only made conditional on quality.
    #[test]
    fn intact_silent_sink_still_wins() {
        let renders = [
            ep("Speakers (Realtek HD Audio)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let p = probe(vec![
            ("steam streaming microphone", fmt(48_000, 2)),
            ("realtek", fmt(48_000, 2)),
        ]);
        let w = plan_with_formats(
            &renders,
            &[],
            None,
            false,
            &p,
            2,
            &[],
            &MintedIds::default(),
        );
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
    }

    /// Narrow audio still beats NO audio: with nothing else available the narrowing sink is taken
    /// and flagged, not refused.
    #[test]
    fn narrowing_sink_is_taken_when_it_is_all_there_is() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let p = probe(vec![("steam streaming microphone", fmt(16_000, 1))]);
        let w = plan_with_formats(
            &renders,
            &[],
            None,
            false,
            &p,
            2,
            &[],
            &MintedIds::default(),
        );
        assert_eq!(
            w.loopback_render.as_ref().unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
        let why = w.loopback_narrowing.expect("must be flagged");
        assert!(why.contains("16000"), "{why}");
    }

    /// Real hardware can narrow too — a headset in its hands-free profile is 16 kHz mono — and
    /// must be flagged just the same. The flag is about the CHOSEN endpoint, not about which tier
    /// it came from.
    #[test]
    fn narrowing_is_reported_for_real_hardware_too() {
        let renders = [ep("Headset (Hands-Free AG Audio)")];
        let p = probe(vec![("headset", fmt(16_000, 1))]);
        let w = plan_with_formats(
            &renders,
            &[],
            None,
            false,
            &p,
            2,
            &[],
            &MintedIds::default(),
        );
        assert_eq!(
            w.loopback_render.as_ref().unwrap().0,
            "Headset (Hands-Free AG Audio)"
        );
        assert!(w.loopback_narrowing.is_some());
    }

    /// An unknown format must never make the plan WORSE than it was before formats existed: a
    /// probe that answers nothing has to reproduce `plan` exactly.
    #[test]
    fn unknown_formats_reproduce_the_formatless_plan() {
        let renders = [
            ep("Speakers (Apple Audio Device)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Speakers)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let captures = [ep("CABLE Output (VB-Audio Virtual Cable)")];
        for host_audio in [false, true] {
            let a = plan(
                &renders,
                &captures,
                None,
                host_audio,
                &[],
                &MintedIds::default(),
            );
            let b = plan_with_formats(
                &renders,
                &captures,
                None,
                host_audio,
                &no_formats,
                2,
                &[],
                &MintedIds::default(),
            );
            assert_eq!(a, b, "host_audio={host_audio}");
            assert!(a.loopback_narrowing.is_none());
        }
    }

    /// `host_audio` still prefers real hardware, and a narrowing silent sink stays last in that
    /// mode too.
    #[test]
    fn host_audio_ordering_survives_formats() {
        let renders = [
            ep("Speakers (Realtek HD Audio)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let p = probe(vec![
            ("steam streaming microphone", fmt(24_000, 1)),
            ("realtek", fmt(48_000, 2)),
        ]);
        let w = plan_with_formats(&renders, &[], None, true, &p, 2, &[], &MintedIds::default());
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    /// The narrowing test is channel-count aware: an endpoint that is fine for stereo narrows a
    /// 5.1 session.
    #[test]
    fn narrowing_depends_on_the_session_channel_count() {
        let stereo_only = fmt(48_000, 2);
        assert_eq!(stereo_only.narrowing(2), None);
        assert!(stereo_only.narrowing(6).is_some());
        // Rate is judged independently of channels.
        assert!(fmt(44_100, 8).narrowing(2).is_some());
        // And an endpoint wider than the session is never "narrowing".
        assert_eq!(fmt(48_000, 8).narrowing(2), None);
        // Both wrong: the message must name both problems.
        let both = fmt(16_000, 1).narrowing(6).unwrap();
        assert!(both.contains("16000") && both.contains("channel"), "{both}");
    }

    /// The recording-default hygiene picker: skips every virtual capture (cable, streaming mic,
    /// the minted "Punktfunk" pair, VoiceMeeter) and lands on the real microphone — the exact
    /// recording-tab zoo of the 2026-08-14 Helldivers 2 field box.
    #[test]
    fn recording_hygiene_picks_the_real_microphone() {
        let captures = [
            ep("Microphone (2- Punktfunk)"),
            ep("CABLE Output (VB-Audio Virtual Cable)"),
            ep("Microphone (Steam Streaming Microphone)"),
            ep("VoiceMeeter Output (VB-Audio VoiceMeeter VAIO)"),
            ep("Desktop Microphone (2- Microsoft LifeCam HD-3000)"),
        ];
        assert_eq!(
            real_capture(&captures, None).unwrap().0,
            "Desktop Microphone (2- Microsoft LifeCam HD-3000)"
        );
        // `avoid_id` guards the plan's own mic capture even when its name would pass the
        // virtual test; with nothing else real, the answer is honestly None.
        let only = [ep("Desk Mic (USB)")];
        assert!(real_capture(&only, Some("id-desk mic (usb)")).is_none());
        assert!(real_capture(&[], None).is_none());
    }

    /// Operator override beats the candidate order.
    #[test]
    fn env_override_wins() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Voicemeeter Input (VB-Audio Voicemeeter VAIO)"),
        ];
        let captures = [ep("Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)")];
        let w = plan(
            &renders,
            &captures,
            Some("voicemeeter input"),
            false,
            &[],
            &MintedIds::default(),
        );
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
        let w = plan(&renders, &[], None, false, &[], &MintedIds::default());
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
            let w = plan(
                &renders,
                &captures,
                None,
                host_audio,
                &[],
                &MintedIds::default(),
            );
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
            let w = plan(&renders, &[], None, host_audio, &[], &MintedIds::default());
            assert!(w.mic_render.is_some(), "host_audio={host_audio}");
            assert!(w.loopback_render.is_none(), "host_audio={host_audio}");
        }
    }

    /// A generically-"virtual" leftover (unknown vendor cable) is refused too: the last resort
    /// accepts ONLY the Steam Streaming Speakers, so a virtual endpoint that slips past
    /// `excluded_from_loopback`'s name list still can't become the loopback.
    #[test]
    fn unknown_virtual_never_loopback() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Some Virtual Audio Device)"),
        ];
        let w = plan(&renders, &[], None, false, &[], &MintedIds::default());
        assert!(w.loopback_render.is_none());
    }

    /// The fingerprint keys the capture loop's "wait for an endpoint change" state: it must
    /// ignore enumeration order (Windows churns it), react to any topology change, and never
    /// alias the render and capture directions.
    #[test]
    fn fingerprint_order_independent_topology_sensitive() {
        let a = [ep("Speakers (Realtek HD Audio)"), ep("CABLE Input")];
        let a_rev = [ep("CABLE Input"), ep("Speakers (Realtek HD Audio)")];
        let caps = [ep("CABLE Output")];
        assert_eq!(fingerprint(&a, &caps), fingerprint(&a_rev, &caps));
        assert_ne!(fingerprint(&a, &caps), fingerprint(&a[..1], &caps));
        assert_ne!(fingerprint(&a, &caps), fingerprint(&caps, &a));
    }

    /// The unsatisfiable-plan diagnosis must name what the mic reserved and advise ONLY the
    /// remedies not already taken: in the field case the Steam pair was installed (so "install
    /// Steam" would point at a fix the box already had) and the cable was missing (so VB-CABLE
    /// is the advice that actually frees the silent sink).
    #[test]
    fn describe_no_loopback_skips_satisfied_remedies() {
        // Mic PINNED to the Streaming Microphone by operator override — the only way the mic
        // may strand the loopback now that game audio outranks the candidate order — with
        // nothing else present.
        let renders = [ep("Altavoces (Steam Streaming Microphone)")];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let w = plan(
            &renders,
            &captures,
            Some("steam streaming microphone"),
            false,
            &[],
            &MintedIds::default(),
        );
        assert!(w.loopback_unsatisfiable());
        let msg = describe_no_loopback(&renders, &w);
        assert!(msg.contains("reserved for the virtual mic"), "{msg}");
        assert!(msg.contains("VB-Audio Virtual Cable"), "{msg}");
        assert!(!msg.contains("install Steam"), "{msg}");

        // Cable-only headless box: VB-CABLE is already installed (and freeing it wouldn't help
        // anyway), while the Steam pair is the remedy that adds a capturable sink.
        let renders = [ep("CABLE Input (VB-Audio Virtual Cable)")];
        let captures = [ep("CABLE Output (VB-Audio Virtual Cable)")];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert!(w.loopback_unsatisfiable());
        let msg = describe_no_loopback(&renders, &w);
        assert!(msg.contains("install Steam"), "{msg}");
        assert!(!msg.contains("install VB-Audio Virtual Cable"), "{msg}");
    }

    /// A stamped pad endpoint is invisible to the plan. Its name carries NO virtual marker — on
    /// purpose, games must read it as the pad's speaker — so the name rules alone would classify
    /// it as real hardware and hand it the loopback; only the id exclusion prevents that.
    /// Measured fact: the wiring plan on the target box already enumerated a stamped endpoint.
    #[test]
    fn pad_endpoints_invisible() {
        let renders = [
            ep("DualSense Wireless Controller"),
            ep("Speakers (Realtek HD Audio)"),
        ];
        let pads = [renders[0].1.clone()];
        let w = plan(&renders, &[], None, false, &pads, &MintedIds::default());
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
        // Even an operator mic override matching the pad's name must not claim it; with the
        // pad as the only render endpoint there is honestly no mic target and no loopback.
        let w = plan(
            &renders[..1],
            &[],
            Some("wireless controller"),
            false,
            &pads,
            &MintedIds::default(),
        );
        assert!(w.mic_render.is_none());
        assert!(w.loopback_render.is_none());
    }

    /// The exclusion has to survive the LAST RESORT tier, which this merge introduced alongside
    /// pad audio. `last_resort` matches on the Steam-Speakers name, but it reads the same
    /// shadowed `renders`, so a pad can never be reached through it either — otherwise the whole
    /// desktop mix would be routed into the controller's voice coils.
    #[test]
    fn a_pad_is_never_the_last_resort() {
        // Only the pad and the Steam pair exist, the mic PINNED to the Streaming Microphone by
        // operator override (game audio otherwise outranks the mic and takes the endpoint), so
        // the plan falls all the way through to the last resort.
        let renders = [
            ep("DualSense Wireless Controller"),
            ep("Speakers (Steam Streaming Microphone)"),
            ep("Speakers (Steam Streaming Speakers)"),
        ];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let pads = [renders[0].1.clone()];
        let w = plan(
            &renders,
            &captures,
            Some("steam streaming microphone"),
            false,
            &pads,
            &MintedIds::default(),
        );
        assert_eq!(
            w.loopback_render.as_ref().unwrap().0,
            "Speakers (Steam Streaming Speakers)",
            "the last resort must skip the pad"
        );
        assert!(w.loopback_last_resort);

        // …and with the pad as the ONLY candidate left, the plan stays honestly unsatisfiable
        // rather than falling back onto the coils.
        let w = plan(
            &renders[..1],
            &captures,
            None,
            false,
            &pads,
            &MintedIds::default(),
        );
        assert!(
            w.loopback_render.is_none(),
            "a pad was taken as the last resort"
        );
        assert!(!w.loopback_last_resort);
        assert!(w.loopback_unsatisfiable());
    }

    // ---- minted tier-0 (the audio-substrate program) -------------------------------------

    /// The minted zoo: both punktfunk instances present alongside the primaries, real
    /// hardware, AND a cable — deliberately name-identical to the primaries, because that is
    /// what the driver produces (S1 measured the confusion).
    fn minted_zoo() -> ([Endpoint; 6], [Endpoint; 3], MintedIds) {
        let renders = [
            ep("Speakers (Realtek HD Audio)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Lautsprecher (Steam Streaming Speakers)"),
            ep("Lautsprecher (Steam Streaming Microphone)"),
            (
                "Lautsprecher (Steam Streaming Speakers)".into(),
                "id-minted-spk".into(),
            ),
            (
                "Lautsprecher (Steam Streaming Microphone)".into(),
                "id-minted-mic-r".into(),
            ),
        ];
        let captures = [
            ep("CABLE Output (VB-Audio Virtual Cable)"),
            ep("Mikrofon (Steam Streaming Microphone)"),
            (
                "Mikrofon (Steam Streaming Microphone)".into(),
                "id-minted-mic-c".into(),
            ),
        ];
        let minted = MintedIds {
            speakers_render: Some("id-minted-spk".into()),
            mic_render: Some("id-minted-mic-r".into()),
            mic_capture: Some("id-minted-mic-c".into()),
        };
        (renders, captures, minted)
    }

    /// The end-state: with the minted pair present, the mic takes its own device and the
    /// loopback takes the minted sink — by ID, ignoring the name-identical primaries, the
    /// cable, and real hardware. Both features coexist without VB-Cable, client-only silent.
    #[test]
    fn minted_pair_is_tier_zero() {
        let (renders, captures, minted) = minted_zoo();
        let w = plan(&renders, &captures, None, false, &[], &minted);
        assert_eq!(w.mic_render.as_ref().unwrap().1, "id-minted-mic-r");
        assert_eq!(
            w.mic_capture.as_ref().unwrap().1,
            "id-minted-mic-c",
            "the capture side must pair by the provider's id, never by name"
        );
        assert_eq!(w.loopback_render.as_ref().unwrap().1, "id-minted-spk");
        assert!(!w.loopback_last_resort);
        assert!(!w.mic_withheld);
        assert_eq!(readiness(&w), AudioReadiness::Full);
    }

    /// `host_audio` still prefers real hardware for the loopback; the mic keeps its minted
    /// device either way.
    #[test]
    fn minted_host_audio_prefers_hardware() {
        let (renders, captures, minted) = minted_zoo();
        let w = plan(&renders, &captures, None, true, &[], &minted);
        assert_eq!(w.mic_render.as_ref().unwrap().1, "id-minted-mic-r");
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    /// The operator override still beats the minted mic — an explicit choice wins everything.
    #[test]
    fn env_override_beats_minted() {
        let (renders, captures, minted) = minted_zoo();
        let w = plan(
            &renders,
            &captures,
            Some("cable input"),
            false,
            &[],
            &minted,
        );
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
        // The minted sink still serves the loopback.
        assert_eq!(w.loopback_render.unwrap().1, "id-minted-spk");
    }

    /// Partial mint (speakers only — the SSM leg failed): the mic falls back to the name
    /// ladder, the loopback keeps the minted sink. Nothing regresses below today's behavior.
    #[test]
    fn minted_speakers_only_mic_uses_ladder() {
        let (renders, captures, mut minted) = minted_zoo();
        minted.mic_render = None;
        minted.mic_capture = None;
        let w = plan(&renders, &captures, None, false, &[], &minted);
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
        assert_eq!(w.loopback_render.unwrap().1, "id-minted-spk");
    }

    /// A minted id the enumeration no longer serves must not produce a phantom assignment —
    /// the plan falls back to the ladder exactly as if nothing were minted.
    #[test]
    fn stale_minted_ids_fall_back() {
        let renders = [
            ep("Speakers (Realtek HD Audio)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
        ];
        let captures = [ep("CABLE Output (VB-Audio Virtual Cable)")];
        let minted = MintedIds {
            speakers_render: Some("id-gone".into()),
            mic_render: Some("id-gone-too".into()),
            mic_capture: Some("id-gone-three".into()),
        };
        let a = plan(&renders, &captures, None, false, &[], &minted);
        let b = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(a, b);
    }

    /// A minted sink that NARROWS the mix demotes below real hardware like any silent sink —
    /// tier-0 is an identity privilege, not a quality exemption.
    #[test]
    fn minted_sink_narrowing_demotes() {
        let renders = [
            ep("Speakers (Realtek HD Audio)"),
            (
                "Lautsprecher (Steam Streaming Speakers)".into(),
                "id-minted-spk".into(),
            ),
        ];
        let minted = MintedIds {
            speakers_render: Some("id-minted-spk".into()),
            ..Default::default()
        };
        let p = probe(vec![("steam streaming", fmt(16_000, 1))]);
        let w = plan_with_formats(&renders, &[], None, false, &p, 2, &[], &minted);
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    /// The readiness classification the log line (and later the status API) carries.
    #[test]
    fn readiness_table() {
        let (renders, captures, minted) = minted_zoo();
        let full = plan(&renders, &captures, None, false, &[], &minted);
        assert_eq!(readiness(&full), AudioReadiness::Full);
        // Steam-pair-only, no cable: audio yes (withheld mic), mic no.
        let renders = [ep("Altavoces (Steam Streaming Microphone)")];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(readiness(&w), AudioReadiness::AudioOnly);
        // Cable-only headless: mic yes, audio no.
        let renders = [ep("CABLE Input (VB-Audio Virtual Cable)")];
        let captures = [ep("CABLE Output (VB-Audio Virtual Cable)")];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(readiness(&w), AudioReadiness::MicOnly);
        // Nothing at all.
        let w = plan(&[], &[], None, false, &[], &MintedIds::default());
        assert_eq!(readiness(&w), AudioReadiness::Nothing);
    }
}
