//! Minted punktfunk-owned audio endpoints — the Windows audio substrate.
//!
//! The audio-substrate decision (`windows-audio-endpoints-and-vbcable.md`, 2026-08-07, spikes
//! S2+S3 measured green): instead of borrowing Steam's primary endpoints and bundling VB-Cable
//! for the mic, the host mints its OWN instances of Valve's streaming-audio drivers —
//!
//! * **"Punktfunk Speakers"** (`SteamStreamingSpeakers.inf`): the client-only loopback sink.
//!   Desktop audio routes here (the wiring plan parks the default playback on it during a
//!   stream), its WASAPI loopback feeds the encoder, and the host stays silent. Measured
//!   clean: 48 kHz stereo f32, loopback peak == rendered peak (S2).
//! * **"Punktfunk Microphone"** (`SteamStreamingMicrophone.inf`): the virtual mic. The host
//!   writes the client's decoded voice into its render side; its capture side surfaces as the
//!   microphone host apps record. Measured bit-faithful render→capture (S3).
//!
//! Provisioning mirrors the pad-audio provider: a background worker at host start, idempotent
//! devnode-per-role with a durable `PunktfunkAudioRole` marker in `Device Parameters` (names
//! are NOT identity — a minted instance is name-identical to Steam's primaries), results
//! published once for the wiring plan to consume BY ID ([`minted_ids`] →
//! [`wiring_plan::MintedIds`]). Everything is best-effort: no Steam driver, a denied install,
//! or `PUNKTFUNK_NO_AUDIO_MINT` leaves the ids empty and the wiring plan falls back to the
//! name-based ladder (Steam primaries → cable → real hardware) unchanged.
//!
//! Endpoints are PERSISTENT by design, like pad endpoints — they survive host restarts and
//! re-resolve by marker on the next start. `punktfunk-host audio-probe` carries the manual
//! `mint` / `plan` inspection paths.

use super::pad_endpoint as pe;
use super::{audio_control, wiring_plan};
use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Durable role marker in a minted devnode's `Device Parameters` key.
const ROLE_MARKER: &str = "PunktfunkAudioRole";
/// How long to wait for audiosrv to register a freshly minted endpoint.
const ENDPOINT_WAIT: Duration = Duration::from_secs(15);
/// Minimum spacing between provisioning retries once the startup attempt failed
/// ([`ensure_provisioned`] is called from wiring passes, which recur freely).
const RETRY_COOLDOWN: Duration = Duration::from_secs(60);

/// The two minted roles. `value` is the persisted marker; the needles drive
/// [`discover_driver`].
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

/// The provider's published result. Partial is possible and usable (one driver leg failing
/// must not cost the other role); consumers read the per-role `Option`s.
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

/// Set once by the worker, and only when at least one role provisioned (the pad provider's R5
/// lesson: latching an empty result turns one transient failure into a process-lifetime
/// disability).
static PROVISIONED: OnceLock<Arc<MintedAudio>> = OnceLock::new();
/// A provisioning attempt is in flight — keeps concurrent askers to one worker.
static PROVISIONING: AtomicBool = AtomicBool::new(false);
/// When the last attempt STARTED — the [`RETRY_COOLDOWN`] anchor.
static LAST_ATTEMPT: Mutex<Option<Instant>> = Mutex::new(None);

/// The wiring plan's tier-0 input: the minted ids, or all-empty while nothing is provisioned.
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

/// Spawn the provisioning worker (idempotent; returns immediately). Called at host start next
/// to the pad provider, and again from [`ensure_provisioned`] on the retry path.
pub(crate) fn provision_at_startup() {
    if std::env::var_os("PUNKTFUNK_NO_AUDIO_MINT").is_some() {
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
                Ok(_) => tracing::info!(
                    "no minted audio endpoints (Steam's streaming drivers absent?) — the \
                     wiring plan keeps the name-based ladder"
                ),
                Err(e) => tracing::warn!(error = %format!("{e:#}"),
                    "minted-audio provisioning failed — the wiring plan keeps the name-based \
                     ladder and a later wiring pass retries"),
            }
            PROVISIONING.store(false, Ordering::SeqCst);
        });
    if let Err(e) = spawned {
        PROVISIONING.store(false, Ordering::SeqCst);
        tracing::warn!(error = %e, "could not spawn the minted-audio provisioning thread");
    }
}

/// Retry hook for wiring passes: cheap once latched; while unlatched it re-asks at most every
/// [`RETRY_COOLDOWN`] — a box where Steam arrives later mints on a later pass instead of at
/// the next reboot.
pub(crate) fn ensure_provisioned() {
    if PROVISIONED.get().is_some() {
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

/// One synchronous provisioning pass over both roles (worker thread + the `audio-probe mint`
/// devtest). Per-role failures degrade to that role being absent.
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

/// Ensure one role's devnode + endpoint(s): reuse the marker-matched devnode from an earlier
/// run, else mint one; (re)bind the driver idempotently; wait for audiosrv's endpoints; put
/// back any default device the fresh endpoint grabbed (measured on the pad program: a newly
/// registered endpoint can take either default).
fn ensure_role(role: Role) -> Result<(String, String, Option<String>)> {
    let prev_render = audio_control::default_render_id();
    let prev_capture = audio_control::default_capture_id();

    let (hwid, inf) = discover_driver(role.needle(), role.inf_name())?;
    let devnode = match find_role_devnode(role)? {
        Some(inst) => inst,
        None => {
            let inst = pe::create_media_devnode(role.desc(), &hwid, |set, did| {
                pe::write_devparam_dword(set, did, ROLE_MARKER, role.value())
            })?;
            tracing::info!(role = role.label(), devnode = %inst, "minted an audio devnode");
            inst
        }
    };
    pe::bind_driver(&hwid, &inf)?;

    let render = wait_for(&devnode, false)?;
    let capture = match role {
        Role::Mic => Some(wait_for(&devnode, true).with_context(|| {
            format!("the minted mic devnode {devnode} produced no capture endpoint")
        })?),
        Role::Speakers => None,
    };

    // Stamp the human name onto every endpoint of the role. Field-measured necessity, not
    // cosmetics: unstamped, the minted instances read "Lautsprecher (2- Steam Streaming
    // Microphone)" etc. and even the box's owner picked the wrong device out of the Sound
    // settings zoo. Names only (a wider stamp set makes AudioEndpointBuilder re-mint the
    // endpoint under a new GUID — the pad program measured that); stamping needs the SYSTEM
    // ACL route on the MMDevices keys, so a dev-run devtest may leave the names unstamped —
    // the wiring never depends on them (identity is the recorded id).
    for ep in [Some(&render), capture.as_ref()].into_iter().flatten() {
        stamp_name(ep, role);
    }

    // Freshly registered endpoints can grab a default; the wiring plan owns default policy,
    // not the mint.
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

/// How many stamp/settle passes a name gets before we accept "stored but not yet served"
/// (a settled endpoint takes the stamp on the first pass; a freshly minted one may need the
/// audio stack to notice — it serves after the next Audiosrv restart/reboot at the latest).
const STAMP_ATTEMPTS: usize = 3;
/// Settle time between a stamp write and its served-check (mirrors the pad provisioner:
/// checking immediately reports success on passes that later get reverted).
const STAMP_SETTLE: Duration = Duration::from_millis(1200);

/// Best-effort: write the role's display name onto one endpoint and wait for the audio stack
/// to SERVE it. Never fails the role — an unnamed endpoint still wires correctly by id.
fn stamp_name(endpoint_id: &str, role: Role) {
    let stamps = [
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
    // Steady state (every boot after the first): the names are already served — no writes,
    // no settle sleeps.
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

/// Poll audiosrv for the endpoint a minted devnode registers in one direction.
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

/// The devnode a previous run minted for `role` (marker-matched — names are not identity).
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

/// Find the (exact hardware id, INF path) for one of Steam's streaming drivers: prefer any
/// installed devnode whose hardware-id list contains `needle` (its `oemNN.inf` is the driver
/// Windows already trusts), else fall back to Steam's driver directory. Shared with the
/// `audio-probe` devtest.
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
        // Devnode exists but its INF is gone — keep its exact hwid, try Steam's directory.
        if let Some(s) = steam_dir_inf() {
            return Ok((hwid, s));
        }
    }
    // No installed devnode at all: canonical hwid + Steam's directory.
    if let Some(s) = steam_dir_inf() {
        return Ok((format!("ROOT\\{}", inf_name.trim_end_matches(".inf")), s));
    }
    bail!(
        "no installed devnode matches {needle:?} and Steam's driver directory has no \
         {inf_name} — install Steam (it never needs to run)"
    )
}

/// `audio-probe mint` devtest body: one synchronous provisioning pass, results printed.
/// Synchronous provisioning — for the mic pump's resolve and the devtests.
///
/// The pump's FIRST open must not race the startup worker: measured on the target box, the
/// pump wired 2 s before the worker latched, took the cable as its write target, and the next
/// wiring pass would then have pointed the default recording at the minted microphone —
/// which nothing writes into: dead mic-air until a pump reopen. Blocking the first resolve
/// (existing marker devnodes re-resolve in milliseconds; a cold boot pays the one-time mint)
/// keeps the pump's target and the plan's verdict the same thing. Latched calls return
/// immediately; the opt-out env is honoured like everywhere else.
pub(crate) fn ensure_blocking() {
    if std::env::var_os("PUNKTFUNK_NO_AUDIO_MINT").is_some() || PROVISIONED.get().is_some() {
        return;
    }
    if let Ok(m) = ensure_all() {
        if m.any() {
            let _ = PROVISIONED.set(Arc::new(m));
        }
    }
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
