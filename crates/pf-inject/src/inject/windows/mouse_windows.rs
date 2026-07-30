//! Resident virtual HID mouse on Windows via the UMDF minidriver (`packaging/windows/drivers/pf-mouse`).
//!
//! **Why**: with no pointing device attached (a headless streaming box — no dongle), win32k reports
//! the cursor as absent (`GetSystemMetrics(SM_MOUSEPRESENT)` = 0) and DWM never composites a cursor
//! into the pf-vdisplay frame — the streamed desktop has an invisible pointer even though
//! `SendInput` moves it. Keeping ONE virtual HID mouse devnode alive for the host's lifetime makes
//! Windows always consider a pointer present and draw the cursor — the Sunshine/Parsec-class fix,
//! with zero client changes. Injection stays [`super::sendinput`]; the report path here is
//! exercised by `punktfunk-host vmouse-spike` (on-glass validation) and is the future
//! higher-fidelity injection route.
//!
//! Transport is the **sealed pad channel** verbatim ([`PadChannel`],
//! `design/gamepad-channel-sealing.md`): an unnamed 64-B `MouseShm` DATA section the host
//! duplicates into the driver's WUDFHost, bootstrapped via the named `Global\pfmouse-boot-0`
//! mailbox. The devnode is `SwDeviceCreate`'d like a pad but held for the PROCESS lifetime (the
//! [`ensure_resident`] thread never drops it), so the pointer survives across sessions; it
//! disappears with the host service, which is exactly when nobody is streaming.

use super::dualsense_windows::{create_swdevice, SwDeviceProfile};
use super::gamepad_raii::{DriverAttach, PadChannel, ProofTransport};
use anyhow::Result;
use pf_driver_proto::mouse::{input_report, mouse_boot_name, MouseShm, MOUSE_MAGIC};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

const SHM_SIZE: usize = core::mem::size_of::<MouseShm>();
const OFF_IN_SEQ: usize = core::mem::offset_of!(MouseShm, in_seq);
const OFF_REPORT: usize = core::mem::offset_of!(MouseShm, report);
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(MouseShm, driver_proto);
const OFF_DRIVER_HEARTBEAT: usize = core::mem::offset_of!(MouseShm, driver_heartbeat);
const OFF_PAD_INDEX: usize = core::mem::offset_of!(MouseShm, pad_index);

/// The one resident virtual mouse: the `SwDeviceCreate`'d `pf_mouse_0` devnode (the pf-mouse HID
/// minidriver loads on it → Windows counts a pointer present) plus the sealed shared-memory
/// channel. Dropping it removes the devnode — [`ensure_resident`] therefore never drops it.
pub struct VirtualMouse {
    /// Devnode RAII (`SwDeviceClose` on drop). `None` falls back to an out-of-band devnode.
    _sw: Option<super::gamepad_raii::SwDevice>,
    channel: PadChannel,
    attach: DriverAttach,
    seq: u32,
}

impl VirtualMouse {
    /// Create the sealed channel (unnamed DATA section + `Global\pfmouse-boot-0` mailbox), stamp
    /// the index + the magic LAST, then spawn the devnode and eagerly deliver the DATA handle.
    pub fn open() -> Result<VirtualMouse> {
        let boot_name = mouse_boot_name(0);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range. Index
        // first, magic LAST — the same publish order the pads use.
        unsafe {
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, 0u32);
            std::ptr::write_unaligned(base as *mut u32, MOUSE_MAGIC);
        }
        let (hsw, instance_id) = match create_swdevice(&SwDeviceProfile {
            instance: "pf_mouse_0",
            container_tag: 0x5046_4D4F, // "PFMO" — never grouped with a pad's container
            container_index: 0,
            hwid: "pf_mouse",
            // An obviously-virtual identity (PF:MO). The synthesized USB bus tokens are inert for
            // a mouse (nothing fingerprints them); reusing the shared profile keeps one code path.
            usb_vid_pid: "VID_5046&PID_4D4F",
            usb_mi: None,
            description: "Punktfunk Virtual Mouse",
        }) {
            Ok((h, i)) => (Some(h), i),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "SwDeviceCreate failed; falling back to an out-of-band pf_mouse devnode");
                (None, None)
            }
        };
        // The DATA section goes to whoever THIS devnode says is serving it — not to whatever pid
        // the LocalService-writable mailbox names (security-review 2026-07-28).
        channel.bind_devnode(0, instance_id.clone(), ProofTransport::HidSerialString);
        let _sw = hsw.map(super::gamepad_raii::SwDevice::new);
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(VirtualMouse {
            _sw,
            channel,
            attach: DriverAttach::new(
                "pf_mouse",
                "pf_mouse.inf",
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pfmouse-driver.log",
                boot_name,
                instance_id,
            ),
            seq: 0,
        })
    }

    /// Publish an input report (5-bit buttons, absolute 15-bit x/y, wheel/pan deltas) and bump
    /// `in_seq` (Release) — the driver's timer completes a pended `READ_REPORT` with it. Unused by
    /// sessions today (`SendInput` injects); the spike drives it, and a future fidelity mode will.
    pub fn send_report(&mut self, buttons: u8, x: u16, y: u16, wheel: i8, pan: i8) {
        let r = input_report(buttons, x, y, wheel, pan);
        self.seq = self.seq.wrapping_add(1).max(1); // never publish seq 0 (= "nothing yet")
        let base = self.channel.data_base();
        // SAFETY: base points at SHM_SIZE bytes; the report slot is OFF_REPORT..+8 and OFF_IN_SEQ
        // (== 4) is 4-aligned off the page-aligned base, so the AtomicU32 view is valid. The report
        // bytes are published BEFORE the seq (Release) — the driver's Acquire load of `in_seq`
        // therefore observes the matching report.
        unsafe {
            std::ptr::copy_nonoverlapping(r.as_ptr(), base.add(OFF_REPORT), r.len());
            (*(base.add(OFF_IN_SEQ) as *const AtomicU32)).store(self.seq, Ordering::Release);
        }
    }

    /// One service tick: pump the sealed-channel delivery and feed the driver-attach health
    /// watcher (the driver's 8 ms timer stamps `driver_proto` while it has the section mapped).
    pub fn service(&mut self) {
        self.channel.pump();
        self.attach.observe(self.driver_proto());
    }

    fn driver_proto(&self) -> u32 {
        // SAFETY: base points at SHM_SIZE bytes; OFF_DRIVER_PROTO is in range.
        unsafe {
            std::ptr::read_unaligned(self.channel.data_base().add(OFF_DRIVER_PROTO) as *const u32)
        }
    }

    fn driver_heartbeat(&self) -> u32 {
        // SAFETY: base points at SHM_SIZE bytes; OFF_DRIVER_HEARTBEAT is in range.
        unsafe {
            std::ptr::read_unaligned(
                self.channel.data_base().add(OFF_DRIVER_HEARTBEAT) as *const u32
            )
        }
    }
}

/// One pending compose-kick aim, desktop coordinates: the target display's rect plus the
/// virtual-desktop bounds to normalize against (both from CCD, so they describe the CONSOLE's
/// layout whatever session this process is in). Newest-wins single slot — kicks are idempotent
/// damage nudges, queueing them would only multiply pointer blips.
struct KickAim {
    rect: (i32, i32, i32, i32),
    bounds: (i32, i32, i32, i32),
}

struct KickSlot {
    slot: Mutex<Option<KickAim>>,
    wake: Condvar,
}

static KICK: KickSlot = KickSlot {
    slot: Mutex::new(None),
    wake: Condvar::new(),
};

/// True while the keeper's mouse is open AND the pf-mouse driver is attached (its 8 ms timer
/// stamps `driver_proto`) — the only state in which a kick's reports actually reach win32k.
static MOUSE_READY: AtomicBool = AtomicBool::new(false);

/// Request a pointer jiggle on the given display through the resident virtual mouse — the
/// COMPOSE KICK's reliable arm. A report from a HID device is REAL input to win32k: it wakes a
/// powered-off display subsystem (lid-closed / display idle-off / modern standby), resets idle
/// timers, counts as user presence, and is delivered regardless of the calling process's session
/// or the active desktop — every condition under which the `SendInput` kick is silently impotent
/// (wrong session → wrong input queue; secure desktop → blocked; display-off → nothing to
/// damage). Asynchronous: the keeper thread (which owns the one process-wide mouse) executes it
/// within its tick. Returns `false` when the resident mouse isn't up (opted out, driver not
/// installed, not yet attached) — the caller falls back to `SendInput`.
pub(crate) fn hid_kick(rect: (i32, i32, i32, i32), bounds: (i32, i32, i32, i32)) -> bool {
    if !MOUSE_READY.load(Ordering::Relaxed) {
        return false;
    }
    *KICK.slot.lock().unwrap() = Some(KickAim { rect, bounds });
    KICK.wake.notify_one();
    true
}

/// Execute one compose kick on the keeper thread: park the pointer at the target rect's center,
/// dwell one composition interval, wiggle ~2 px, then put it back where it was. Every report is
/// device-level input (see [`hid_kick`]). The dwell is load-bearing (the Stage-W3 on-glass
/// finding, same as the SendInput jump path): DWM samples the cursor position at the next vsync
/// tick, so a sub-tick round trip composes nothing. The gaps also respect the driver's 8 ms
/// report timer — back-to-back writes into the single report slot would coalesce.
///
/// The restore is best-effort via `GetCursorPos`: in a wrong-session host it describes the wrong
/// session's pointer, so the console pointer is instead left near the target's center — which is
/// the streamed display, exactly where the pointer is about to be useful.
fn perform_kick(m: &mut VirtualMouse, aim: KickAim) {
    let (bx, by, bw, bh) = aim.bounds;
    if bw <= 0 || bh <= 0 {
        return;
    }
    // Field-log which kick arm fired (the SendInput arm logs in kick_dwm_compose) — a lid-closed
    // repro should show this line followed by the driver's first acquired frame.
    tracing::debug!(
        rect = ?aim.rect,
        bounds = ?aim.bounds,
        "HID compose kick — parking the pointer on the target display (display wake + damage)"
    );
    let map = |px: i32, py: i32| -> (u16, u16) {
        let nx = ((px - bx).clamp(0, bw - 1) as i64 * 0x7FFF) / i64::from(bw - 1).max(1);
        let ny = ((py - by).clamp(0, bh - 1) as i64 * 0x7FFF) / i64::from(bh - 1).max(1);
        (nx as u16, ny as u16)
    };
    let mut p = POINT::default();
    // SAFETY: plain FFI; `p` is a valid out-param for this synchronous call.
    let orig = unsafe { GetCursorPos(&mut p) }
        .is_ok()
        .then_some((p.x, p.y));
    let (rx, ry, rw, rh) = aim.rect;
    let (cx, cy) = map(rx + rw / 2, ry + rh / 2);
    // ~2 desktop pixels in HID units, at least 1 — the wiggle must actually move the pointer.
    let dx = ((2 * 0x7FFF) / bw.max(1)).max(1) as u16;
    m.send_report(0, cx, cy, 0, 0);
    std::thread::sleep(Duration::from_millis(35));
    m.send_report(0, cx.saturating_add(dx).min(0x7FFF), cy, 0, 0);
    std::thread::sleep(Duration::from_millis(35));
    match orig {
        Some((ox, oy)) => {
            let (ox, oy) = map(ox, oy);
            m.send_report(0, ox, oy, 0, 0);
        }
        None => m.send_report(0, cx, cy, 0, 0),
    }
}

/// Make sure the resident virtual mouse exists (idempotent, best-effort). Called whenever an
/// [`InjectorService`](crate::InjectorService) starts — multiple services (native +
/// GameStream) share the ONE process-wide mouse, guarded here. Spawns a keeper thread that owns
/// the devnode for the process lifetime and pumps the channel at a slow tick (delivery is eager at
/// open; the pump only handles a late WUDFHost + feeds the attach diagnostics).
///
/// `PUNKTFUNK_NO_VIRTUAL_MOUSE=1` opts out (diagnostics, or an operator who objects to a virtual
/// pointer device).
pub(crate) fn ensure_resident() {
    use std::sync::OnceLock;
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        if std::env::var_os("PUNKTFUNK_NO_VIRTUAL_MOUSE").is_some_and(|v| v != "0") {
            tracing::info!(
                "virtual HID mouse disabled (PUNKTFUNK_NO_VIRTUAL_MOUSE) — with no physical \
                 pointer attached, Windows will not draw a cursor into the stream"
            );
            return;
        }
        // Hand the capture crate its HID compose-kick hook (the one-way-edge inversion: pf-capture
        // never reaches back into inject). Registered exactly when the resident mouse is being
        // brought up; until the driver actually attaches, `hid_kick` reports not-ready and the
        // kick falls back to SendInput.
        let _ = pf_capture::HID_COMPOSE_KICK.set(hid_kick);
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-vmouse".into())
            .spawn(keeper_thread)
        {
            tracing::warn!(error = %e, "virtual-mouse keeper thread spawn failed");
        }
    });
}

/// Open-with-retry, then hold + pump forever. Open only realistically fails on a mailbox squat
/// (another punktfunk-host instance) — retry slowly; a missing/failed DRIVER is not an open
/// failure (the devnode exists but nothing binds), which [`DriverAttach`] diagnoses via the pump.
/// Each tick also publishes kick-readiness ([`MOUSE_READY`]) and executes at most one pending
/// compose kick ([`hid_kick`]) — the condvar wait keeps kick latency at "immediately", not "next
/// 250 ms tick", while an idle keeper still only wakes 4×/s.
fn keeper_thread() {
    loop {
        match VirtualMouse::open() {
            Ok(mut m) => {
                tracing::info!(
                    "resident virtual HID mouse created (pf_mouse — keeps SM_MOUSEPRESENT true \
                     so DWM composites the cursor on headless hosts)"
                );
                loop {
                    m.service();
                    MOUSE_READY.store(m.driver_proto() != 0, Ordering::Relaxed);
                    let (mut slot, _timeout) = KICK
                        .wake
                        .wait_timeout_while(
                            KICK.slot.lock().unwrap(),
                            Duration::from_millis(250),
                            |k| k.is_none(),
                        )
                        .unwrap();
                    let aim = slot.take();
                    drop(slot);
                    if let Some(aim) = aim {
                        if m.driver_proto() != 0 {
                            perform_kick(&mut m, aim);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "virtual HID mouse open failed — retrying in 60s (headless hosts stream an \
                     invisible cursor until it exists)"
                );
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    }
}

/// `vmouse-spike` (dev validation): hold the virtual mouse and drive the REAL cursor through the
/// HID report path — proves the full chain (SwDeviceCreate → INF bind → mshidumdf → mouhid →
/// win32k) on-glass. Run with the host service STOPPED (the resident mouse owns the mailbox name
/// otherwise). Verify while it holds: `Get-PnpDevice` shows the pf_mouse devnode + a HID child,
/// `GetSystemMetrics(SM_MOUSEPRESENT)` = 1 with no physical mouse, and the cursor sweeps a
/// horizontal line mid-screen.
pub fn spike_hold(secs: u64) -> Result<()> {
    let mut m = VirtualMouse::open()?;
    println!("virtual HID mouse devnode up (5046:4D4F) — waiting for the driver to attach…");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while m.driver_proto() == 0 && std::time::Instant::now() < deadline {
        m.service();
        std::thread::sleep(Duration::from_millis(50));
    }
    if m.driver_proto() == 0 {
        println!(
            "driver never attached (10s). Install it: punktfunk-host.exe driver install --gamepad \
             --dir <stage>  (pf_mouse.inf ships with the gamepad drivers); see the WARN above."
        );
    } else {
        println!(
            "driver attached (proto {}). Sweeping the cursor for {secs}s — watch the glass: the \
             pointer should glide left↔right across mid-screen; wheel ticks every second.",
            m.driver_proto()
        );
    }
    let t0 = std::time::Instant::now();
    let mut i: u64 = 0;
    let beat_before = m.driver_heartbeat();
    while t0.elapsed() < Duration::from_secs(secs) {
        // Triangle-wave X sweep over the middle 3/4 of the axis, fixed mid-screen Y; one wheel
        // tick per second so scroll delivery is visible too.
        let phase = (i % 240) as i32; // 240 steps × 16 ms ≈ 4 s per round trip
        let tri = if phase < 120 { phase } else { 240 - phase };
        let x = 4096 + (tri as u32 * (24576 / 120)) as u16;
        let wheel: i8 = if i % 60 == 0 { 1 } else { 0 };
        m.send_report(0, x, 0x4000, wheel, 0);
        m.service();
        i += 1;
        std::thread::sleep(Duration::from_millis(16));
    }
    let beat = m.driver_heartbeat();
    println!(
        "vmouse-spike: done (driver heartbeat advanced {} ticks — {}). Devnode removed on exit.",
        beat.wrapping_sub(beat_before),
        if beat != beat_before {
            "driver alive"
        } else {
            "driver NOT ticking"
        }
    );
    Ok(())
}

/// **Channel-proof probe** — settles, on a real box, the one thing the design could not settle by
/// reading: which HID IOCTL hidclass actually forwards to a UMDF HID minidriver.
///
/// The pad channel hands a pad's whole input surface to whichever process the DEVNODE names
/// (`pf_driver_proto::gamepad::ChannelProof`), because the bootstrap mailbox is writable by
/// LocalService and therefore cannot be trusted to name it (security-review 2026-07-28). For the two
/// HID minidrivers that answer travels as a HID string, and both `HidD_GetIndexedString` and a
/// direct arbitrary-index `IOCTL_HID_GET_STRING` are wired up so only one of them has to work. This
/// prints which one did.
///
/// Self-contained: it spins up its OWN throwaway `pf_mouse_probe` devnode at pad index 9, so it can
/// run alongside a live host without touching the resident mouse (index 0) or its mailbox, and the
/// devnode disappears when the command exits. Needs the pf_mouse driver installed
/// (`punktfunk-host.exe driver install --gamepad`).
pub fn channel_proof_probe() -> Result<()> {
    use crate::channel_proof::{self, ProofTransport};

    /// A pad index no real pad uses, so the proof's index check is actually exercised and the
    /// probe can never be confused with the resident mouse at 0.
    const PROBE_INDEX: u8 = 9;

    println!("creating a throwaway pf_mouse devnode (pad index {PROBE_INDEX})…");
    let (hsw, instance_id) = create_swdevice(&SwDeviceProfile {
        instance: "pf_mouse_probe",
        container_tag: 0x5046_4D4F, // "PFMO"
        container_index: PROBE_INDEX,
        hwid: "pf_mouse",
        usb_vid_pid: "VID_5046&PID_4D4F",
        usb_mi: None,
        description: "Punktfunk Virtual Mouse (channel-proof probe)",
    })?;
    let _sw = super::gamepad_raii::SwDevice::new(hsw);
    let Some(instance_id) = instance_id else {
        anyhow::bail!("SwDeviceCreate reported no instance id — cannot look the devnode up");
    };

    // PnP has to start the driver and hidclass has to publish the collection interface; both happen
    // in tens of milliseconds, but poll rather than sleep a fixed amount so a slow box still reports.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let report = loop {
        let r = channel_proof::diagnose(
            &instance_id,
            ProofTransport::HidSerialString,
            PROBE_INDEX as u32,
        );
        if r.contains("ChannelProof") || std::time::Instant::now() >= deadline {
            break r;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    println!("\n{report}");

    match channel_proof::probe_pid(
        &instance_id,
        ProofTransport::HidSerialString,
        PROBE_INDEX as u32,
    ) {
        Ok(pid) => println!(
            "RESULT: the devnode proved its driver is pid {pid} — the HID channel proof WORKS on \
             this build of Windows, so the pad channel never has to trust the mailbox."
        ),
        Err(e) => println!(
            "RESULT: no usable channel proof ({e:#}).\n\
             If BOTH HID lines above say \"call failed\", hidclass on this build forwards neither \
             IOCTL to a UMDF minidriver and the HID pads/mouse need a different transport (the \
             xusb leg is unaffected — it owns its own device interface). If one says \"answered, \
             but not a proof\", an OLD pf_mouse driver is installed: reinstall with\n\
             \x20  punktfunk-host.exe driver install --gamepad"
        ),
    }
    Ok(())
}
