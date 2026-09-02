//! Minted punktfunk-owned audio endpoints — own instances of Valve's streaming-audio
//! drivers, not Steam's primaries and not a bundled VB-Cable.
//!
//! * Speakers (`SteamStreamingSpeakers.inf`): silent host sink; WASAPI loopback feeds the encoder.
//! * Microphone (`SteamStreamingMicrophone.inf`): host writes decoded voice to render; capture
//!   is the mic host apps record.
//!
//! A background worker at host start mints one marked `ROOT\MEDIA` devnode per role
//! (`PunktfunkAudioRole` in Device Parameters — names match Steam's primaries, so identity is
//! the marker). [`minted_ids`] publishes the endpoint ids for the wiring plan. Missing Steam
//! drivers, a denied install, or `PUNKTFUNK_NO_AUDIO_MINT` leaves the ids empty and the plan
//! stays on the name-based ladder.
//!
//! Endpoints persist across host restarts and re-resolve by marker. Evidence:
//! `design/windows-audio-endpoints-and-vbcable.md`. Probe: `punktfunk-host audio-probe mint`.

use super::pad_endpoint as pe;
use super::{audio_control, wiring_plan};
use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Durable `Device Parameters` marker. The uninstall sweep matches minted devnodes on it.
pub(crate) const ROLE_MARKER: &str = "PunktfunkAudioRole";
/// Audiosrv can take this long to register a freshly minted endpoint.
const ENDPOINT_WAIT: Duration = Duration::from_secs(15);
/// Floor between retries. [`ensure_provisioned`] is called from wiring passes, which recur freely.
const RETRY_COOLDOWN: Duration = Duration::from_secs(60);
/// Unlatched PnP (re)binds broadcast a device-change every running app services.
/// A box that cannot mint must not pay that on every retry; a service restart re-arms.
const MAX_UNLATCHED_ATTEMPTS: u32 = 5;
/// Wait for an in-flight pass. A cold mint worst-cases around two [`ENDPOINT_WAIT`]s plus stamp settles.
const BLOCKING_WAIT: Duration = Duration::from_secs(90);

/// Persisted marker (`value`) and the INF/hwid needles for [`discover_driver`].
#[derive(Clone, Copy, PartialEq)]
enum Role {
    Speakers,
    Mic,
}

impl Role {
    fn value(self) -> u32 {
        match self {
            Role::Speakers => 1,
            Role::Mic => 2,
        }
    }
    fn desc(self) -> &'static str {
        match self {
            Role::Speakers => "Punktfunk Speakers",
            Role::Mic => "Punktfunk Microphone",
        }
    }
    fn needle(self) -> &'static str {
        match self {
            Role::Speakers => "steamstreamingspeakers",
            Role::Mic => "steamstreamingmicrophone",
        }
    }
    fn inf_name(self) -> &'static str {
        match self {
            Role::Speakers => "SteamStreamingSpeakers.inf",
            Role::Mic => "SteamStreamingMicrophone.inf",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Role::Speakers => "speakers",
            Role::Mic => "mic",
        }
    }
}

/// Partial is usable: one driver leg failing must not cost the other role.
#[derive(Debug, Default, Clone)]
pub(crate) struct MintedAudio {
    pub speakers_devnode: Option<String>,
    pub speakers_render: Option<String>,
    pub mic_devnode: Option<String>,
    pub mic_render: Option<String>,
    pub mic_capture: Option<String>,
}

impl MintedAudio {
    fn any(&self) -> bool {
        self.speakers_render.is_some() || self.mic_render.is_some()
    }
}

/// Latch only a non-empty result. An empty latch freezes a transient failure for the process lifetime.
static PROVISIONED: OnceLock<Arc<MintedAudio>> = OnceLock::new();
/// Keeps concurrent askers to one worker.
static PROVISIONING: AtomicBool = AtomicBool::new(false);
/// When the last attempt started — the [`RETRY_COOLDOWN`] anchor, not when it finished.
static LAST_ATTEMPT: Mutex<Option<Instant>> = Mutex::new(None);
/// Give-up counter for [`MAX_UNLATCHED_ATTEMPTS`], shared by the worker and the blocking path.
static UNLATCHED_ATTEMPTS: AtomicU32 = AtomicU32::new(0);

/// Count one finished-but-unlatched pass. The crossing attempt logs the give-up exactly once.
fn record_unlatched_attempt() {
    let n = UNLATCHED_ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
    if n == MAX_UNLATCHED_ATTEMPTS {
        tracing::warn!(
            attempts = n,
            "minted-audio provisioning keeps failing — giving up for this host lifetime so \
             retries stop broadcasting device changes at the whole box; the wiring plan keeps \
             the name-based ladder, a service restart re-arms minting"
        );
    }
}

fn gave_up() -> bool {
    UNLATCHED_ATTEMPTS.load(Ordering::SeqCst) >= MAX_UNLATCHED_ATTEMPTS
}

/// Wiring-plan tier-0: minted endpoint ids, or all-empty while nothing is provisioned.
pub(crate) fn minted_ids() -> wiring_plan::MintedIds {
    match PROVISIONED.get() {
        Some(m) => wiring_plan::MintedIds {
            speakers_render: m.speakers_render.clone(),
            mic_render: m.mic_render.clone(),
            mic_capture: m.mic_capture.clone(),
        },
        None => wiring_plan::MintedIds::default(),
    }
}

/// The full record including devnode instance ids. [`minted_ids`] is the wiring-plan subset.
pub(crate) fn provisioned() -> Option<Arc<MintedAudio>> {
    PROVISIONED.get().cloned()
}

/// Spawn the provisioning worker. Idempotent; returns immediately.
pub(crate) fn provision_at_startup() {
    if std::env::var_os("PUNKTFUNK_NO_AUDIO_MINT").is_some() || gave_up() {
        return;
    }
    if PROVISIONED.get().is_some() || PROVISIONING.swap(true, Ordering::SeqCst) {
        return;
    }
    *LAST_ATTEMPT.lock().unwrap() = Some(Instant::now());
    let spawned = thread::Builder::new()
        .name("punktfunk-audio-mint".into())
        .spawn(|| {
            match ensure_all() {
                Ok(m) if m.any() => {
                    tracing::info!(
                        speakers = m.speakers_render.as_deref().unwrap_or("-"),
                        mic_render = m.mic_render.as_deref().unwrap_or("-"),
                        mic_capture = m.mic_capture.as_deref().unwrap_or("-"),
                        "minted audio endpoints ready (the wiring plan's tier-0)"
                    );
                    let _ = PROVISIONED.set(Arc::new(m));
                }
                Ok(_) => {
                    tracing::info!(
                        "no minted audio endpoints (Steam's streaming drivers absent?) — the \
                         wiring plan keeps the name-based ladder"
                    );
                    record_unlatched_attempt();
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"),
                        "minted-audio provisioning failed — the wiring plan keeps the name-based \
                         ladder and a later wiring pass retries");
                    record_unlatched_attempt();
                }
            }
            PROVISIONING.store(false, Ordering::SeqCst);
        });
    if let Err(e) = spawned {
        PROVISIONING.store(false, Ordering::SeqCst);
        tracing::warn!(error = %e, "could not spawn the minted-audio provisioning thread");
    }
}

/// Wiring-pass retry. Cheap once latched; while unlatched, at most every [`RETRY_COOLDOWN`] so a late Steam install still mints.
pub(crate) fn ensure_provisioned() {
    if PROVISIONED.get().is_some() || gave_up() {
        return;
    }
    {
        let last = LAST_ATTEMPT.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < RETRY_COOLDOWN) {
            return;
        }
    }
    provision_at_startup();
}

/// One pass over both roles. A per-role failure leaves that role absent rather than failing the pair.
fn ensure_all() -> Result<MintedAudio> {
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA, minted-audio)")?;
    let mut out = MintedAudio::default();
    for role in [Role::Speakers, Role::Mic] {
        match ensure_role(role) {
            Ok((devnode, render, capture)) => match role {
                Role::Speakers => {
                    out.speakers_devnode = Some(devnode);
                    out.speakers_render = Some(render);
                }
                Role::Mic => {
                    out.mic_devnode = Some(devnode);
                    out.mic_render = Some(render);
                    out.mic_capture = capture;
                }
            },
            Err(e) => tracing::info!(role = role.label(), error = %format!("{e:#}"),
                "minted-audio role unavailable"),
        }
    }
    Ok(out)
}

/// One role: reuse a marked healthy devnode, else mint; (re)bind; wait for audiosrv. A fresh endpoint can steal either default — put it back.
fn ensure_role(role: Role) -> Result<(String, String, Option<String>)> {
    // Healthy marked endpoints: return without a driver (re)bind. That bind broadcasts
    // a device-change every running app services; games rebuild their audio graph on it.
    if let Some((devnode, render, capture)) = find_healthy_role(role)? {
        stamp_identity(&render, role, false);
        if let Some(cap) = capture.as_ref() {
            stamp_identity(cap, role, true);
        }
        return Ok((devnode, render, capture));
    }

    let prev_render = audio_control::default_render_id();
    let prev_capture = audio_control::default_capture_id();

    let (hwid, inf) = discover_driver(role.needle(), role.inf_name())?;
    let devnode = match find_role_devnode(role)? {
        Some(inst) => inst,
        // Register-then-mark can leave a live unmarked devnode. Adopt it before minting
        // a second; the uninstall sweep only sees marked instances.
        None => match adopt_orphan_devnode(role, &hwid)? {
            Some(inst) => inst,
            None => {
                let inst = pe::create_media_devnode(role.desc(), &hwid, |set, did| {
                    pe::write_devparam_dword(set, did, ROLE_MARKER, role.value())
                })?;
                tracing::info!(role = role.label(), devnode = %inst, "minted an audio devnode");
                inst
            }
        },
    };
    pe::bind_driver(&hwid, &inf)?;

    let render = wait_for(&devnode, false)?;
    let capture = match role {
        Role::Mic => Some(wait_for(&devnode, true).with_context(|| {
            format!("the minted mic devnode {devnode} produced no capture endpoint")
        })?),
        Role::Speakers => None,
    };

    stamp_identity(&render, role, false);
    if let Some(cap) = capture.as_ref() {
        stamp_identity(cap, role, true);
    }

    // A freshly registered endpoint can grab a default. The wiring plan owns that policy, not the mint.
    if let Some(prev) = prev_render {
        if audio_control::default_render_id().as_deref() != Some(prev.as_str())
            && audio_control::set_default_endpoint(&prev).is_ok()
        {
            tracing::info!(
                role = role.label(),
                "default playback restored after minting"
            );
        }
    }
    if let Some(prev) = prev_capture {
        if audio_control::default_capture_id().as_deref() != Some(prev.as_str())
            && audio_control::set_default_endpoint(&prev).is_ok()
        {
            tracing::info!(
                role = role.label(),
                "default recording restored after minting"
            );
        }
    }
    Ok((devnode, render, capture))
}

/// Marker-matched devnode with every endpoint the role owes already registered, else `None` so the caller runs the full pass.
fn find_healthy_role(role: Role) -> Result<Option<(String, String, Option<String>)>> {
    let Some(devnode) = find_role_devnode(role)? else {
        return Ok(None);
    };
    let Some(render) = pe::find_endpoint_for_devnode(&devnode)? else {
        return Ok(None);
    };
    let capture = match role {
        Role::Mic => match pe::find_capture_endpoint_for_devnode(&devnode)? {
            Some(cap) => Some(cap),
            None => return Ok(None),
        },
        Role::Speakers => None,
    };
    Ok(Some((devnode, render, capture)))
}

/// Stamp/settle passes before accepting "stored but not yet served". A settled endpoint takes the first; a fresh one may wait for Audiosrv.
const STAMP_ATTEMPTS: usize = 3;
/// Gap between a stamp write and the served-check. Immediate reads report success on writes the stack later reverts.
const STAMP_SETTLE: Duration = Duration::from_millis(1200);

/// Stereo 48 kHz f32 `WAVEFORMATEXTENSIBLE` — both sides of the minted mic declare this mix format.
///
/// The driver forwards the render stream raw into capture. A mono-stamped render is unopenable
/// (`AUDCLNT_E_UNSUPPORTED_FORMAT`); a stereo render against a mono capture default plays an
/// octave low. Pin both sides coherent.
const WFX_F32_2CH_48K: [u8; 40] = [
    0xfe, 0xff, // wFormatTag = WAVE_FORMAT_EXTENSIBLE
    0x02, 0x00, // nChannels = 2
    0x80, 0xbb, 0x00, 0x00, // nSamplesPerSec = 48000
    0x00, 0xdc, 0x05, 0x00, // nAvgBytesPerSec = 384000
    0x08, 0x00, // nBlockAlign = 8
    0x20, 0x00, // wBitsPerSample = 32
    0x16, 0x00, // cbSize = 22
    0x20, 0x00, // wValidBitsPerSample = 32
    0x03, 0x00, 0x00, 0x00, // dwChannelMask = FL | FR
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
    0x71, // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
];
/// Device-format leg: 16-bit PCM stereo. A float device-format is one of the incoherent sets that make the endpoint unopenable.
const WFX_PCM16_2CH_48K: [u8; 40] = [
    0xfe, 0xff, // wFormatTag = WAVE_FORMAT_EXTENSIBLE
    0x02, 0x00, // nChannels = 2
    0x80, 0xbb, 0x00, 0x00, // nSamplesPerSec = 48000
    0x00, 0xee, 0x02, 0x00, // nAvgBytesPerSec = 192000
    0x04, 0x00, // nBlockAlign = 4
    0x10, 0x00, // wBitsPerSample = 16
    0x16, 0x00, // cbSize = 22
    0x10, 0x00, // wValidBitsPerSample = 16
    0x03, 0x00, 0x00, 0x00, // dwChannelMask = FL | FR
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
    0x71, // KSDATAFORMAT_SUBTYPE_PCM
];

/// Best-effort display name and, on the mic, the stereo format set. Never fails the role — wiring keys off the recorded id. A wider stamp makes AudioEndpointBuilder re-mint the endpoint under a new GUID.
fn stamp_identity(endpoint_id: &str, role: Role, capture: bool) {
    let mut stamps = vec![
        pe::Stamp {
            label: "device-desc",
            key: pe::PKEY_DEVICE_DESC,
            value: pe::StampValue::Str(role.desc()),
        },
        pe::Stamp {
            label: "device-name",
            key: pe::PKEY_ENDPOINT_DEVICE_NAME,
            value: pe::StampValue::Str("Punktfunk"),
        },
    ];
    // Both mic pins stereo 48 kHz. Capture gets the device format only — mix/host keys
    // are render-engine properties and break shared-mode Initialize on a capture endpoint.
    if role == Role::Mic {
        stamps.push(pe::Stamp {
            label: "device-format",
            key: pe::PKEY_DEVICE_FORMAT,
            value: pe::StampValue::Format(&WFX_PCM16_2CH_48K),
        });
        if !capture {
            stamps.extend([
                pe::Stamp {
                    label: "mix-format-2",
                    key: pe::PKEY_MIX_FORMAT_2,
                    value: pe::StampValue::Format(&WFX_F32_2CH_48K),
                },
                pe::Stamp {
                    label: "mix-format-3",
                    key: pe::PKEY_MIX_FORMAT_3,
                    value: pe::StampValue::Format(&WFX_F32_2CH_48K),
                },
                pe::Stamp {
                    label: "host-format",
                    key: pe::PKEY_HOST_FORMAT,
                    value: pe::StampValue::Format(&WFX_F32_2CH_48K),
                },
            ]);
        }
    }
    // Already served (every boot after the first): no writes, no settle sleeps.
    if pe::stamps_served(endpoint_id, &stamps) {
        return;
    }
    for attempt in 0..STAMP_ATTEMPTS {
        if let Err(e) = pe::write_stamps(endpoint_id, &stamps) {
            tracing::info!(role = role.label(), endpoint = %endpoint_id,
                error = %format!("{e:#}"),
                "could not stamp the minted endpoint's name (needs the SYSTEM ACL route) — \
                 the endpoint still wires correctly, it just keeps the driver's default name");
            return;
        }
        thread::sleep(STAMP_SETTLE);
        if pe::stamps_served(endpoint_id, &stamps) {
            if attempt > 0 {
                tracing::debug!(
                    role = role.label(),
                    attempt = attempt + 1,
                    "minted endpoint name held after a re-pass"
                );
            }
            return;
        }
    }
    tracing::info!(role = role.label(), endpoint = %endpoint_id,
        "minted endpoint name is stored but not yet served — it appears after the next \
         audio-stack restart or reboot");
}

/// Poll until audiosrv has registered this devnode's render or capture endpoint.
fn wait_for(devnode: &str, capture: bool) -> Result<String> {
    let deadline = Instant::now() + ENDPOINT_WAIT;
    loop {
        let found = if capture {
            pe::find_capture_endpoint_for_devnode(devnode)?
        } else {
            pe::find_endpoint_for_devnode(devnode)?
        };
        if let Some(ep) = found {
            return Ok(ep);
        }
        if Instant::now() >= deadline {
            bail!(
                "no {} endpoint appeared for {devnode} within {}s — is Audiosrv running?",
                if capture { "capture" } else { "render" },
                ENDPOINT_WAIT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// Marker-matched devnode from a previous run. Names match Steam's primaries, so they are not identity.
fn find_role_devnode(role: Role) -> Result<Option<String>> {
    let set = pe::media_class_devs()?;
    for i in 0.. {
        let mut did = pe::devinfo_data();
        // SAFETY: live set; `did` is a live out-param with cbSize set.
        if unsafe {
            windows::Win32::Devices::DeviceAndDriverInstallation::SetupDiEnumDeviceInfo(
                set.0, i, &mut did,
            )
        }
        .is_err()
        {
            break;
        }
        if pe::read_devparam_dword(&set, &did, ROLE_MARKER) == Some(role.value()) {
            if let Some(inst) = pe::instance_id(&set, &did) {
                return Ok(Some(inst));
            }
        }
    }
    Ok(None)
}

/// Reclaim a `ROOT\MEDIA\NNNN` instance of this role's hwid that carries no owner marker.
///
/// Minting is register-then-mark; a host that dies between leaves a live, unmarked, driver-bound
/// endpoint. The next pass would mint a duplicate the uninstall sweep cannot see. Steam's own
/// devices are `ROOT\SteamStreamingSpeakers\*` / `ROOT\SteamStreamingMicrophone\*` — same hwid,
/// never that instance prefix — so this cannot adopt them. Any family marker is left alone.
fn adopt_orphan_devnode(role: Role, hwid: &str) -> Result<Option<String>> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiEnumDeviceInfo, SPDRP_HARDWAREID,
    };
    let set = pe::media_class_devs()?;
    for i in 0.. {
        let mut did = pe::devinfo_data();
        // SAFETY: live set; `did` is a live out-param with cbSize set.
        if unsafe { SetupDiEnumDeviceInfo(set.0, i, &mut did) }.is_err() {
            break; // ERROR_NO_MORE_ITEMS
        }
        let Some(inst) = pe::instance_id(&set, &did) else {
            continue;
        };
        if !inst.to_ascii_uppercase().starts_with("ROOT\\MEDIA\\") {
            continue;
        }
        if !pe::devnode_multi_sz_prop(&set, &did, SPDRP_HARDWAREID)
            .iter()
            .any(|h| h.eq_ignore_ascii_case(hwid))
        {
            continue;
        }
        if super::devnode_cleanup::OWNER_MARKERS
            .iter()
            .any(|m| pe::read_devparam_dword(&set, &did, m).is_some())
        {
            continue;
        }
        pe::write_devparam_dword(&set, &mut did, ROLE_MARKER, role.value())?;
        tracing::warn!(
            role = role.label(),
            devnode = %inst,
            "adopted an abandoned audio devnode — one of ours whose owner marker never landed \
             (a host that died mid-mint). Re-marked and reused instead of minting a duplicate"
        );
        return Ok(Some(inst));
    }
    Ok(None)
}

/// Hardware id + INF for one Steam streaming driver: prefer an installed `oemNN.inf` Windows already trusts, else Steam's driver directory.
pub(crate) fn discover_driver(needle: &str, inf_name: &str) -> Result<(String, String)> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiEnumDeviceInfo, SPDRP_HARDWAREID,
    };
    let steam_dir_inf = || -> Option<String> {
        let w = super::wasapi_mic::steam_driver_inf_path(inf_name)?;
        let s = String::from_utf16_lossy(&w)
            .trim_end_matches('\0')
            .to_string();
        std::path::Path::new(&s).exists().then_some(s)
    };
    let set = pe::media_class_devs()?;
    for i in 0.. {
        let mut did = pe::devinfo_data();
        // SAFETY: live set; `did` is a live out-param with cbSize set.
        if unsafe { SetupDiEnumDeviceInfo(set.0, i, &mut did) }.is_err() {
            break;
        }
        let Some(hwid) = pe::devnode_multi_sz_prop(&set, &did, SPDRP_HARDWAREID)
            .into_iter()
            .find(|h| h.to_lowercase().contains(needle))
        else {
            continue;
        };
        if let Some(inf) = pe::devnode_inf_path(&set, &did) {
            let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
            let full = format!(r"{windir}\INF\{inf}");
            if std::path::Path::new(&full).exists() {
                return Ok((hwid, full));
            }
        }
        // Keep the exact hwid even if this devnode's INF is gone; try Steam's directory.
        if let Some(s) = steam_dir_inf() {
            return Ok((hwid, s));
        }
    }
    // The INF stem is the canonical ROOT hardware id when no matching devnode is installed.
    if let Some(s) = steam_dir_inf() {
        return Ok((format!("ROOT\\{}", inf_name.trim_end_matches(".inf")), s));
    }
    bail!(
        "no installed devnode matches {needle:?} and Steam's driver directory has no \
         {inf_name} — install Steam (it never needs to run)"
    )
}

/// Synchronous pass for the mic pump's first resolve and the devtests.
///
/// The pump's first open must not race the startup worker: wiring the cable, then parking
/// default recording on a minted mic nobody writes, is a dead mic until reopen. Existing
/// marked devnodes re-resolve in milliseconds; a cold boot pays the one-time mint. Latched
/// calls and `PUNKTFUNK_NO_AUDIO_MINT` return immediately.
///
/// Unlatched: wait for an in-flight pass instead of racing SetupAPI; a failed pass repeats
/// at most every [`RETRY_COOLDOWN`]; [`MAX_UNLATCHED_ATTEMPTS`] stops for this host lifetime.
pub(crate) fn ensure_blocking() {
    if std::env::var_os("PUNKTFUNK_NO_AUDIO_MINT").is_some()
        || PROVISIONED.get().is_some()
        || gave_up()
    {
        return;
    }
    // Wait for the in-flight pass rather than racing a second SetupAPI/PnP sweep.
    if PROVISIONING.swap(true, Ordering::SeqCst) {
        let deadline = Instant::now() + BLOCKING_WAIT;
        while PROVISIONING.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        return;
    }
    // First resolve runs; after a failed pass the cooldown answers instead of a re-run.
    let run = {
        let mut last = LAST_ATTEMPT.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < RETRY_COOLDOWN) {
            false
        } else {
            *last = Some(Instant::now());
            true
        }
    };
    if run {
        match ensure_all() {
            Ok(m) if m.any() => {
                let _ = PROVISIONED.set(Arc::new(m));
            }
            _ => record_unlatched_attempt(),
        }
    }
    PROVISIONING.store(false, Ordering::SeqCst);
}

pub(crate) fn devtest_mint() -> Result<()> {
    let m = ensure_all()?;
    println!(
        "audio-mint: speakers devnode={} render={}",
        m.speakers_devnode.as_deref().unwrap_or("-"),
        m.speakers_render.as_deref().unwrap_or("-")
    );
    println!(
        "audio-mint: mic devnode={} render={} capture={}",
        m.mic_devnode.as_deref().unwrap_or("-"),
        m.mic_render.as_deref().unwrap_or("-"),
        m.mic_capture.as_deref().unwrap_or("-")
    );
    if m.any() {
        let _ = PROVISIONED.set(Arc::new(m));
        println!(
            "audio-mint: published for this process — `audio-probe plan` shows the tier-0 pick"
        );
    } else {
        println!("audio-mint: nothing minted (Steam's streaming drivers absent?)");
    }
    Ok(())
}
