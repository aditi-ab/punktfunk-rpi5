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
use super::gamepad_raii::{DriverAttach, PadChannel};
use anyhow::Result;
use pf_driver_proto::mouse::{input_report, mouse_boot_name, MouseShm, MOUSE_MAGIC};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

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
            description: "punktfunk Virtual Mouse",
        }) {
            Ok((h, i)) => (Some(h), i),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "SwDeviceCreate failed; falling back to an out-of-band pf_mouse devnode");
                (None, None)
            }
        };
        let _sw = hsw.map(super::gamepad_raii::SwDevice::new);
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(VirtualMouse {
            _sw,
            channel,
            attach: DriverAttach::new(
                "pf_mouse",
                "pf_mouse.inf",
                "C:\\Users\\Public\\pfmouse-driver.log",
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

/// Make sure the resident virtual mouse exists (idempotent, best-effort). Called whenever an
/// [`InjectorService`](crate::inject::InjectorService) starts — multiple services (native +
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
                    std::thread::sleep(Duration::from_millis(250));
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
