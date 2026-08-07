//! `audio-probe` devtest — the spike measurements behind the Windows audio-substrate decision
//! (punktfunk-planning `design/windows-audio-endpoints-and-vbcable.md` §3), runnable over ssh
//! with no game and no client:
//!
//! * `ssm` — **S3, the decision gate.** Mint a SECOND devnode of Valve's Steam Streaming
//!   *Microphone* driver and prove the pair end to end: a tone rendered into the new
//!   instance's render endpoint must come back out of its capture endpoint. Passing means a
//!   punktfunk-owned virtual mic needs no VB-Cable on any box with Steam installed —
//!   failing reverts the drop-VB-Cable decision to "cable stays, mic-only".
//! * `sink` — **S2.** Mint a Steam Streaming *Speakers* instance, park the DEFAULT playback
//!   device on it (the real product routing), render a tone through the *default* device, and
//!   WASAPI-loopback the instance — the desktop-audio capture path minus the game.
//! * `sss-primary` — **S1, informative.** Tone + loopback on the PRIMARY Steam Streaming
//!   Speakers endpoint: the "loopback is silent (validated live)" verdict, re-measured, with
//!   the endpoint's engine mix format and whether Steam is running recorded alongside.
//! * `cleanup` — remove every devnode this probe ever minted.
//!
//! Probe devnodes carry `PunktfunkAudioProbe=1` in their `Device Parameters` key so cleanup
//! finds them without guessing by name (DeviceDesc only survives until the INF installs).
//! Nothing here is product wiring: the wiring plan treats a minted instance like any other
//! endpoint of that name, and the probe restores the default playback/recording devices it
//! disturbed before exiting.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::pad_endpoint as pe;
use super::{audio_control, SAMPLE_RATE};
use anyhow::{anyhow, bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use wasapi::{Direction, SampleType, StreamMode, WaveFormat};
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiEnumDeviceInfo, SetupDiOpenDevRegKey, DICS_FLAG_GLOBAL, DIREG_DEV,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegQueryValueExW, RegSetValueExW, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD,
    REG_VALUE_TYPE,
};

/// Marker value in a probe devnode's `Device Parameters` key — how `cleanup` finds what this
/// devtest minted (and nothing else).
const PROBE_MARKER: &str = "PunktfunkAudioProbe";
/// DeviceDesc for probe devnodes (visible in Device Manager until the INF install renames it).
const PROBE_DESC: &str = "Punktfunk Audio Probe";
/// How long to wait for audiosrv to register a minted endpoint.
const ENDPOINT_WAIT: Duration = Duration::from_secs(15);
/// Tone amplitude — matches `pad-endpoint tone`, so peaks compare across probes.
const TONE_AMP: f32 = 0.5;
/// A measured peak above this is "signal" (tone renders at 0.5; autoconvert may attenuate).
const SIGNAL_FLOOR: f32 = 0.05;

pub(crate) fn run(args: &[String]) -> Result<()> {
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")?;
    let keep = args.iter().any(|a| a == "--keep");
    match args.get(1).map(String::as_str) {
        Some("ssm") => probe_ssm(keep),
        Some("sink") => probe_sink(keep),
        Some("sss-primary") => {
            let secs = args
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(4u32)
                .clamp(2, 30);
            probe_sss_primary(secs)
        }
        Some("cleanup") => cleanup(),
        // The provider's synchronous pass: mint (or re-find) "Punktfunk Speakers/Microphone"
        // and publish them for THIS process — `plan` then shows the tier-0 pick.
        Some("mint") => super::minted::devtest_mint(),
        // One real wiring pass (no default parking) + the verdict, readiness included — the
        // field-triage "what would the host do right now" command. Provisioning runs
        // synchronously first: a fresh CLI process would otherwise race its own worker.
        Some("plan") => {
            super::minted::ensure_blocking();
            let plan = super::audio_control::wire_now_full(false);
            let w = &plan.wiring;
            let show = |ep: &Option<super::wiring_plan::Endpoint>| match ep {
                Some((name, id)) => format!("{name:?} ({id})"),
                None => "-".into(),
            };
            println!("audio-plan: mic_render    = {}", show(&w.mic_render));
            println!("audio-plan: mic_capture   = {}", show(&w.mic_capture));
            println!("audio-plan: loopback      = {}", show(&w.loopback_render));
            println!("audio-plan: last_resort   = {}", w.loopback_last_resort);
            println!("audio-plan: mic_withheld  = {}", w.mic_withheld);
            println!(
                "audio-plan: narrowing     = {}",
                w.loopback_narrowing.as_deref().unwrap_or("-")
            );
            println!(
                "audio-plan: readiness     = {:?}",
                super::wiring_plan::readiness(w)
            );
            Ok(())
        }
        // The pitch instrument for the LIVE minted mic pair (field report: voice through the
        // minted microphone played back "way lower"): a 440 Hz tone into the minted mic's
        // render side, frequency-measured off its capture side. ~440 Hz = the pair is honest;
        // ~220 Hz = a link runs at half the declared rate (the octave-down voice).
        Some("micpitch") => {
            super::minted::ensure_blocking();
            // The RAW provisioning record: the wiring-facing `minted_ids` deliberately hides
            // the mic pair (raw crossing, octave-low — this probe is how that was measured).
            let Some(m) = super::minted::provisioned() else {
                bail!("nothing minted on this box — run `audio-probe mint` first");
            };
            let (Some(render), Some(capture)) = (m.mic_render.clone(), m.mic_capture.clone())
            else {
                bail!("no minted microphone pair on this box — run `audio-probe mint` first");
            };
            println!("audio-probe micpitch: render={render}");
            println!("audio-probe micpitch: capture={capture}");
            let (peak, hz) = tone_while(&Some(render), 6, 440.0, || record_peak(&capture, 4))??;
            println!("audio-probe micpitch: peak={peak:.4}, 440 Hz read back as {hz:.0} Hz");
            if peak < SIGNAL_FLOOR {
                println!("  VERDICT: no signal crossed the pair — is the mic pump holding it?");
            } else if (hz - 440.0).abs() < 40.0 {
                println!("  VERDICT: pitch-true — the minted pair is innocent; the shift lives elsewhere.");
            } else if (hz - 220.0).abs() < 30.0 {
                println!(
                    "  VERDICT: OCTAVE DOWN — the driver forwards the stereo render stream \
                     into the mono capture raw; the render side must run MONO."
                );
            } else {
                println!("  VERDICT: off-pitch by an unusual ratio — measure again / check rates.");
            }
            Ok(())
        }
        // The driver-capability map for the minted mic pair: exclusive+shared
        // IsFormatSupported across {1,2}ch × {16,32}bit × {44.1,48,96}kHz on BOTH pins —
        // interrogates the DRIVER, bypassing every endpoint-store stamping question. What the
        // pins truly accept decides whether the mic leg has any coherent configuration (and
        // whether an exclusive-mode mono open is an escape hatch).
        Some("micpins") => {
            super::minted::ensure_blocking();
            let Some(m) = super::minted::provisioned() else {
                bail!("nothing minted on this box — run `audio-probe mint` first");
            };
            let (Some(render), Some(capture)) = (m.mic_render.clone(), m.mic_capture.clone())
            else {
                bail!("no minted microphone pair on this box");
            };
            for (label, id) in [("render", &render), ("capture", &capture)] {
                println!("audio-probe micpins: {label} = {id}");
                let device = pe::open_wasapi_device(id)?;
                let client = device.get_iaudioclient().context("IAudioClient")?;
                for ch in [1usize, 2] {
                    for bits in [16usize, 32] {
                        for rate in [44_100usize, 48_000, 96_000] {
                            let stype = if bits == 16 {
                                SampleType::Int
                            } else {
                                SampleType::Float
                            };
                            let fmt = WaveFormat::new(bits, bits, &stype, rate, ch, None);
                            let mut verdicts = Vec::new();
                            for (mode_label, mode) in [
                                ("excl", wasapi::ShareMode::Exclusive),
                                ("shared", wasapi::ShareMode::Shared),
                            ] {
                                let v = match client.is_supported(&fmt, &mode) {
                                    Ok(None) => "OK",
                                    Ok(Some(_)) => "alt",
                                    Err(_) => "no",
                                };
                                verdicts.push(format!("{mode_label}={v}"));
                            }
                            println!("  {ch}ch {bits:2}bit {rate:5}Hz  {}", verdicts.join(" "));
                        }
                    }
                }
            }
            Ok(())
        }
        _ => bail!(
            "usage: punktfunk-host audio-probe \
             <ssm|sink|sss-primary|mint|plan|micpitch|micpins|cleanup> [--keep]"
        ),
    }
}

// --- S3: minted Steam Streaming Microphone instance ----------------------------------------

fn probe_ssm(keep: bool) -> Result<()> {
    let (hwid, inf) = discover_driver("steamstreamingmicrophone", "SteamStreamingMicrophone.inf")?;
    println!("audio-probe ssm: hwid={hwid} inf={inf}");
    let prev_render = audio_control::default_render_id();
    let prev_capture = audio_control::default_capture_id();

    let inst = pe::create_media_devnode(PROBE_DESC, &hwid, write_probe_marker)?;
    println!("audio-probe ssm: created devnode {inst}");
    pe::bind_driver(&hwid, &inf)?;

    let render_ep = wait_endpoint(&inst, Dir::Render)?;
    let capture_ep = match wait_endpoint(&inst, Dir::Capture) {
        Ok(ep) => ep,
        Err(e) => {
            // The load-bearing failure shape: an instance that minted a render side but no
            // capture side cannot be a virtual mic — say it precisely, then clean up.
            println!("audio-probe ssm: render endpoint {render_ep} appeared, but:");
            println!("  {e:#}");
            println!("  VERDICT: FAIL (S3) — the minted SSM instance has NO capture endpoint;");
            println!("  a punktfunk-owned virtual mic cannot come from this driver.");
            restore_defaults(prev_render, prev_capture);
            if !keep {
                remove_devnode(&inst);
            }
            return Ok(());
        }
    };
    println!("audio-probe ssm: render={render_ep}");
    println!("audio-probe ssm: capture={capture_ep}");
    report_mix_format("render", &render_ep);

    // E2E: tone into the instance's render side, recorded from its capture side. Concurrent —
    // the driver only moves audio while both ends are open.
    let (peak, hz) = tone_while(&Some(render_ep.clone()), 5, 440.0, || {
        record_peak(&capture_ep, 3)
    })??;
    println!(
        "audio-probe ssm: capture peak over 3s = {peak:.4}, tone 440 Hz read back as {hz:.0} Hz"
    );
    if peak > SIGNAL_FLOOR {
        println!(
            "  VERDICT: PASS (S3) — the minted Steam Streaming Microphone instance carries \
             audio render→capture; a punktfunk-owned virtual mic needs no VB-Cable where \
             Steam is installed."
        );
    } else {
        println!(
            "  VERDICT: FAIL (S3) — both endpoints minted but no audio crossed the pair \
             (peak {peak:.4} ≤ {SIGNAL_FLOOR}); the drop-VB-Cable decision reverts to \
             cable-for-mic-only."
        );
    }

    restore_defaults(prev_render, prev_capture);
    if keep {
        println!("audio-probe ssm: --keep — devnode {inst} left in place");
    } else {
        remove_devnode(&inst);
    }
    Ok(())
}

// --- S2: minted Speakers instance as the parked default sink -------------------------------

fn probe_sink(keep: bool) -> Result<()> {
    let (hwid, inf) = discover_driver("steamstreamingspeakers", "SteamStreamingSpeakers.inf")?;
    println!("audio-probe sink: hwid={hwid} inf={inf}");
    let prev_render = audio_control::default_render_id();
    let prev_capture = audio_control::default_capture_id();

    let inst = pe::create_media_devnode(PROBE_DESC, &hwid, write_probe_marker)?;
    println!("audio-probe sink: created devnode {inst}");
    pe::bind_driver(&hwid, &inf)?;
    let ep = wait_endpoint(&inst, Dir::Render)?;
    println!("audio-probe sink: endpoint={ep}");
    report_mix_format("sink", &ep);

    // The product routing, not a shortcut: default playback parked on the minted endpoint, the
    // tone rendered through the DEFAULT device (as any app would), the loopback reading the
    // minted endpoint. This is `wasapi_cap`'s Assert shape minus the game.
    audio_control::set_default_endpoint(&ep).context("park the default playback on the sink")?;
    let (peak, hz) = tone_while(&None, 5, 440.0, || loopback_peak(&ep, 3))??;
    println!(
        "audio-probe sink: loopback peak over 3s = {peak:.4}, tone 440 Hz read back as {hz:.0} Hz"
    );
    if peak > SIGNAL_FLOOR {
        println!(
            "  VERDICT: PASS (S2) — default-routed audio reaches the minted Speakers instance \
             and its WASAPI loopback carries it; \"Punktfunk Speakers\" can be the canonical \
             client-only sink."
        );
    } else {
        println!(
            "  VERDICT: FAIL (S2) — the minted instance's loopback stayed silent \
             (peak {peak:.4} ≤ {SIGNAL_FLOOR}) despite default routing; the speakers leg of \
             Phase 2 dies and Phase 1 remains the fix."
        );
    }

    restore_defaults(prev_render, prev_capture);
    if keep {
        println!("audio-probe sink: --keep — devnode {inst} left in place");
    } else {
        remove_devnode(&inst);
    }
    Ok(())
}

// --- S1: the primary Steam Streaming Speakers loopback, re-measured ------------------------

fn probe_sss_primary(secs: u32) -> Result<()> {
    // The PRIMARY endpoint: name-matched, but never a devnode this probe minted (a leftover
    // `--keep` instance would shadow the measurement).
    let probes = probe_devnodes()?;
    let en = wasapi::DeviceEnumerator::new().map_err(|e| anyhow!("DeviceEnumerator: {e}"))?;
    let coll = en
        .get_device_collection(&Direction::Render)
        .map_err(|e| anyhow!("render collection: {e}"))?;
    let n = coll.get_nbr_devices().map_err(|e| anyhow!("count: {e}"))?;
    let mut target: Option<(String, String)> = None;
    for i in 0..n {
        let Ok(dev) = coll.get_device_at_index(i) else {
            continue;
        };
        let name = dev.get_friendlyname().unwrap_or_default();
        let id = dev.get_id().unwrap_or_default();
        if name.to_lowercase().contains("steam streaming speakers")
            && !probes
                .iter()
                .any(|(pi, _)| endpoint_of(pi) == Some(id.clone()))
        {
            target = Some((name, id));
            break;
        }
    }
    let Some((name, id)) = target else {
        bail!("no primary Steam Streaming Speakers render endpoint on this box");
    };
    let steam_running = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq steam.exe", "/NH"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains("steam.exe")
        })
        .unwrap_or(false);
    println!(
        "audio-probe sss-primary: endpoint {name:?} ({id}), steam.exe running: {steam_running}"
    );
    report_mix_format("primary", &id);

    let (peak, hz) = tone_while(&Some(id.clone()), secs + 1, 440.0, || {
        loopback_peak(&id, secs)
    })??;
    println!("audio-probe sss-primary: loopback peak over {secs}s = {peak:.4}, tone 440 Hz read back as {hz:.0} Hz");
    if peak > SIGNAL_FLOOR {
        println!(
            "  VERDICT: the primary SSS loopback CARRIES audio here (steam.exe running: \
             {steam_running}) — the \"validated silent\" verdict does not reproduce in this \
             state; record the state alongside."
        );
    } else {
        println!(
            "  VERDICT: the primary SSS loopback is SILENT (steam.exe running: \
             {steam_running}) — consistent with the wiring plan's last-resort tier."
        );
    }
    Ok(())
}

// --- driver discovery — shared with the minted provider -------------------------------------

use super::minted::discover_driver;

// --- probe devnode marker + cleanup --------------------------------------------------------

/// Write the probe marker into a fresh devnode's `Device Parameters` key (the `mark` callback
/// of [`pe::create_media_devnode`]).
fn write_probe_marker(
    set: &pe::DevInfoSet,
    did: &mut windows::Win32::Devices::DeviceAndDriverInstallation::SP_DEVINFO_DATA,
) -> Result<()> {
    // SAFETY: live set + element; DIREG_DEV opens (or the create below mints) the devnode's
    // Device Parameters key.
    let opened = unsafe {
        SetupDiOpenDevRegKey(
            set.0,
            did,
            DICS_FLAG_GLOBAL.0,
            0,
            DIREG_DEV,
            KEY_SET_VALUE.0,
        )
    };
    let hkey = match opened {
        Ok(k) => k,
        // SAFETY: same set + element; a fresh devnode has no Device Parameters key yet.
        Err(_) => unsafe {
            windows::Win32::Devices::DeviceAndDriverInstallation::SetupDiCreateDevRegKeyW(
                set.0,
                did,
                DICS_FLAG_GLOBAL.0,
                0,
                DIREG_DEV,
                None,
                PCWSTR::null(),
            )
        }
        .context("create the probe devnode's Device Parameters key")?,
    };
    let name: Vec<u16> = PROBE_MARKER
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: the value name is NUL-terminated and outlives the call; the DWORD bytes travel
    // with the slice.
    let rc = unsafe {
        RegSetValueExW(
            hkey,
            PCWSTR(name.as_ptr()),
            None,
            REG_DWORD,
            Some(&1u32.to_le_bytes()),
        )
    };
    // SAFETY: closing the key opened/created above, exactly once.
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    rc.ok().context("write PunktfunkAudioProbe")
}

/// Every devnode carrying the probe marker, as `(instance_id, marker_value)`.
fn probe_devnodes() -> Result<Vec<(String, u32)>> {
    let set = pe::media_class_devs()?;
    let mut out = Vec::new();
    for i in 0.. {
        let mut did = pe::devinfo_data();
        // SAFETY: live set; `did` is a live out-param with cbSize set.
        if unsafe { SetupDiEnumDeviceInfo(set.0, i, &mut did) }.is_err() {
            break;
        }
        // SAFETY: live set + element; read-only open of the Device Parameters key.
        let Ok(hkey) = (unsafe {
            SetupDiOpenDevRegKey(
                set.0,
                &did,
                DICS_FLAG_GLOBAL.0,
                0,
                DIREG_DEV,
                KEY_QUERY_VALUE.0,
            )
        }) else {
            continue;
        };
        let name: Vec<u16> = PROBE_MARKER
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut ty = REG_VALUE_TYPE(0);
        let mut data = [0u8; 4];
        let mut len = data.len() as u32;
        // SAFETY: the value name is NUL-terminated; out-params are live locals; the buffer
        // length travels in `len`.
        let rc = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut ty),
                Some(data.as_mut_ptr()),
                Some(&mut len),
            )
        };
        // SAFETY: closing the key opened above, exactly once.
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        if rc.is_ok() && ty == REG_DWORD && len == 4 {
            if let Some(inst) = pe::instance_id(&set, &did) {
                out.push((inst, u32::from_le_bytes(data)));
            }
        }
    }
    Ok(out)
}

fn cleanup() -> Result<()> {
    let probes = probe_devnodes()?;
    if probes.is_empty() {
        println!("audio-probe cleanup: nothing to remove");
        return Ok(());
    }
    for (inst, _) in probes {
        remove_devnode(&inst);
    }
    Ok(())
}

/// `pnputil /remove-device` — same teardown as `pad-endpoint remove`.
fn remove_devnode(inst: &str) {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    match std::process::Command::new(format!(r"{windir}\System32\pnputil.exe"))
        .args(["/remove-device", inst])
        .output()
    {
        Ok(o) if o.status.success() => println!("audio-probe: removed devnode {inst}"),
        Ok(o) => println!(
            "audio-probe: pnputil could not remove {inst} (status {:?}): {}",
            o.status.code(),
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => println!("audio-probe: could not run pnputil for {inst}: {e}"),
    }
}

// --- endpoints ------------------------------------------------------------------------------

enum Dir {
    Render,
    Capture,
}

/// Poll for the endpoint audiosrv registers for `inst` in the given direction.
fn wait_endpoint(inst: &str, dir: Dir) -> Result<String> {
    let deadline = Instant::now() + ENDPOINT_WAIT;
    loop {
        let found = match dir {
            Dir::Render => pe::find_endpoint_for_devnode(inst)?,
            Dir::Capture => pe::find_capture_endpoint_for_devnode(inst)?,
        };
        if let Some(ep) = found {
            return Ok(ep);
        }
        if Instant::now() >= deadline {
            let which = match dir {
                Dir::Render => "render",
                Dir::Capture => "capture",
            };
            bail!(
                "no {which} endpoint appeared for {inst} within {}s",
                ENDPOINT_WAIT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// The render endpoint id of a probe devnode, if it has one (best-effort — S1's exclusion).
fn endpoint_of(inst: &str) -> Option<String> {
    pe::find_endpoint_for_devnode(inst).ok().flatten()
}

fn report_mix_format(label: &str, endpoint_id: &str) {
    match audio_control::mix_format_of(&(label.to_string(), endpoint_id.to_string())) {
        Some(f) => println!(
            "audio-probe: {label} engine mix format = {} Hz, {} ch, {} bits",
            f.rate_hz, f.channels, f.bits
        ),
        None => println!("audio-probe: {label} engine mix format = unknown (probe failed)"),
    }
}

// --- audio movement ------------------------------------------------------------------------

/// Render a stereo tone into `target` (an endpoint id, or the DEFAULT render device for
/// `None`) on a worker thread while `body` runs; the tone stops when `body` returns.
fn tone_while<T>(
    target: &Option<String>,
    tone_secs: u32,
    hz: f32,
    body: impl FnOnce() -> T,
) -> Result<T> {
    let stop = Arc::new(AtomicBool::new(false));
    let (stop_t, target_t) = (stop.clone(), target.clone());
    let join = thread::Builder::new()
        .name("pf-audio-probe-tone".into())
        .spawn(move || render_tone(target_t.as_deref(), tone_secs, hz, &stop_t))
        .context("spawn tone thread")?;
    // Give the render stream a beat to open before measuring, so the measurement window is
    // fully inside the tone.
    thread::sleep(Duration::from_millis(500));
    let out = body();
    stop.store(true, Ordering::SeqCst);
    match join.join() {
        Ok(Ok(())) => Ok(out),
        Ok(Err(e)) => Err(e.context("tone render failed")),
        Err(_) => Err(anyhow!("tone thread panicked")),
    }
}

/// Stereo 48 kHz tone, event-driven shared mode with autoconvert — the same open shape the
/// virtual mic uses, so "the probe could render" transfers.
fn render_tone(target: Option<&str>, seconds: u32, hz: f32, stop: &AtomicBool) -> Result<()> {
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA, tone)")?;
    let device = match target {
        Some(id) => pe::open_wasapi_device(id)?,
        None => wasapi::DeviceEnumerator::new()
            .map_err(|e| anyhow!("DeviceEnumerator: {e}"))?
            .get_default_device(&Direction::Render)
            .map_err(|e| anyhow!("default render device: {e}"))?,
    };
    let mut client = device.get_iaudioclient().context("IAudioClient")?;
    let desired = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, 2, None);
    let (period, _) = client.get_device_period().context("device period")?;
    client
        .initialize_client(
            &desired,
            &Direction::Render,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: period,
            },
        )
        .context("initialize tone render")?;
    let h_event = client.set_get_eventhandle().context("event handle")?;
    let render = client.get_audiorenderclient().context("render client")?;
    let buf_frames = client.get_buffer_size().context("buffer size")? as usize;
    let _ = render.write_to_device(buf_frames, &vec![0u8; buf_frames * 8], None);
    client.start_stream().context("start tone stream")?;

    let total = u64::from(SAMPLE_RATE) * u64::from(seconds.clamp(1, 60));
    let step = std::f32::consts::TAU * hz / SAMPLE_RATE as f32;
    let (mut phase, mut written) = (0.0f32, 0u64);
    let mut bytes = vec![0u8; buf_frames * 8];
    while written < total && !stop.load(Ordering::Relaxed) {
        if h_event.wait_for_event(1000).is_err() {
            bail!("tone render event timed out after {written} frames");
        }
        let free = client.get_available_space_in_frames().context("space")? as usize;
        let n = free.min((total - written) as usize);
        if n == 0 {
            continue;
        }
        for f in 0..n {
            let s = phase.sin() * TONE_AMP;
            phase += step;
            if phase >= std::f32::consts::TAU {
                phase -= std::f32::consts::TAU;
            }
            for c in 0..2 {
                let at = (f * 2 + c) * 4;
                bytes[at..at + 4].copy_from_slice(&s.to_le_bytes());
            }
        }
        render
            .write_to_device(n, &bytes[..n * 8], None)
            .context("write tone")?;
        written += n as u64;
    }
    thread::sleep(Duration::from_millis(200));
    let _ = client.stop_stream();
    Ok(())
}

/// Peak |sample| AND estimated dominant frequency (zero crossings — a pitch-shift detector:
/// a 440 Hz tone reading back as ~220 Hz means some link runs at half the declared rate, which
/// peaks alone can never see) read from an endpoint for `seconds`. `loopback` taps a RENDER
/// endpoint's mix (the desktop-audio capture shape); otherwise a normal record from a CAPTURE
/// endpoint (the virtual-mic consumer shape). MONO request — frequency counting needs a single
/// channel, and autoconvert downmix changes no frequencies.
fn measure_peak(endpoint_id: &str, seconds: u32, loopback: bool) -> Result<(f32, f32)> {
    let device = pe::open_wasapi_device(endpoint_id)?;
    let mut client = device.get_iaudioclient().context("IAudioClient")?;
    let desired = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, 1, None);
    let (period, _) = client.get_device_period().context("device period")?;
    client
        .initialize_client(
            &desired,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: period,
            },
        )
        .with_context(|| {
            format!(
                "initialize {} client",
                if loopback { "loopback" } else { "record" }
            )
        })?;
    let h_event = client.set_get_eventhandle().context("event handle")?;
    let capture = client.get_audiocaptureclient().context("capture client")?;
    client.start_stream().context("start capture stream")?;

    let deadline = Instant::now() + Duration::from_secs(u64::from(seconds.clamp(1, 60)));
    let mut bytes: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
    let mut peak = 0f32;
    let mut frames = 0u64;
    let mut crossings = 0u64;
    let mut prev_positive: Option<bool> = None;
    // Frequency = crossings over the SIGNAL span only (audio starts mid-window; counting the
    // leading silence into the denominator reads every tone low).
    let (mut first_signal, mut last_signal): (Option<u64>, Option<u64>) = (None, None);
    while Instant::now() < deadline {
        let _ = h_event.wait_for_event(100);
        loop {
            match capture.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(_)) => {
                    capture
                        .read_from_device_to_deque(&mut bytes)
                        .context("read capture")?;
                }
                Err(e) => bail!("get_next_packet_size: {e}"),
            }
        }
        let whole = (bytes.len() / 4) * 4;
        if whole > 0 {
            let raw: Vec<u8> = bytes.drain(..whole).collect();
            for c in raw.chunks_exact(4) {
                let s = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                peak = peak.max(s.abs());
                if s.abs() > 0.01 {
                    first_signal.get_or_insert(frames);
                    last_signal = Some(frames);
                    let pos = s > 0.0;
                    if prev_positive.is_some_and(|p| p != pos) {
                        crossings += 1;
                    }
                    prev_positive = Some(pos);
                }
                frames += 1;
            }
        }
    }
    let _ = client.stop_stream();
    let est_hz = match (first_signal, last_signal) {
        (Some(a), Some(b)) if b > a + SAMPLE_RATE as u64 / 10 => {
            crossings as f32 / 2.0 / ((b - a) as f32 / SAMPLE_RATE as f32)
        }
        _ => 0.0,
    };
    println!(
        "audio-probe: {} read {} samples from {endpoint_id} (est {est_hz:.0} Hz)",
        if loopback { "loopback" } else { "record" },
        frames
    );
    Ok((peak, est_hz))
}

fn loopback_peak(endpoint_id: &str, seconds: u32) -> Result<(f32, f32)> {
    measure_peak(endpoint_id, seconds, true)
}

fn record_peak(endpoint_id: &str, seconds: u32) -> Result<(f32, f32)> {
    measure_peak(endpoint_id, seconds, false)
}

/// Put back whatever default devices the minting disturbed (a fresh endpoint can grab either
/// default — measured on the pad program). No-ops when nothing moved.
fn restore_defaults(prev_render: Option<String>, prev_capture: Option<String>) {
    if let Some(prev) = prev_render {
        if audio_control::default_render_id().as_deref() != Some(prev.as_str()) {
            match audio_control::set_default_endpoint(&prev) {
                Ok(()) => println!("audio-probe: default playback restored"),
                Err(e) => println!("audio-probe: could not restore default playback: {e:#}"),
            }
        }
    }
    if let Some(prev) = prev_capture {
        if audio_control::default_capture_id().as_deref() != Some(prev.as_str()) {
            match audio_control::set_default_endpoint(&prev) {
                Ok(()) => println!("audio-probe: default recording restored"),
                Err(e) => println!("audio-probe: could not restore default recording: {e:#}"),
            }
        }
    }
}
