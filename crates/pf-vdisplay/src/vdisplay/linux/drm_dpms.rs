//! Compositor-independent panel darkening over DRM — how a box with **no desktop compositor**
//! honors [`Topology::Exclusive`](crate::policy::Topology::Exclusive).
//!
//! [`crate::panel_dpms`] asks KWin to turn the panels off, which is the right answer whenever there
//! is a KDE desktop to ask. There often isn't. A box sitting in **Game Mode** runs gamescope and no
//! KWin at all, so that path declines — and Game Mode is precisely the deployment where the
//! operator's TV is lit by the box itself. Measured on the Nobara VM (2026-08-24): after the
//! takeover idles the box's gaming session, `card0-HDMI-A-1` sits at `enabled=enabled dpms=On`
//! indefinitely. Nothing blanks on its own — when no client holds DRM master the kernel simply
//! keeps the CRTC configured, and fbcon owns it.
//!
//! So ask the kernel directly. The sequence, all of it measured on that box:
//!
//! 1. `open("/dev/dri/cardN")` — permitted for the ordinary session user, because logind puts a
//!    **uaccess ACL** on the node for whoever holds the active seat (`crw-rw----+`). No root, no
//!    polkit, no group: this is the same access every local compositor gets.
//! 2. `DRM_IOCTL_SET_MASTER` — succeeds while no one else is master, which is exactly the state the
//!    takeover has just produced by idling the box's session. If it FAILS, someone else is driving
//!    that card (a live compositor, a foreign gamescope) and we decline: darkening a panel out from
//!    under its owner is not ours to do, and on the Attach route it would darken the very picture
//!    being streamed.
//! 3. `DRM_IOCTL_MODE_GETRESOURCES` (count pass, then data pass) for the CRTC ids, and
//!    `DRM_IOCTL_MODE_SETCRTC` with `fb_id = 0, mode_valid = 0, count_connectors = 0` on each one
//!    that is actually driving something. That is a modeset to "off": the connector goes
//!    `enabled=disabled dpms=Off`, which is the same end state `kscreen-doctor --dpms off` reaches
//!    through KWin.
//! 4. `DRM_IOCTL_DROP_MASTER`, and **keep the fd open**.
//!
//! Step 4 is the part worth reading twice. The darkness **survives dropping master** (measured), so
//! we hand mastering rights straight back — the box's own gamescope must be able to take the card
//! when the restore relaunches its session, and a host still holding master would starve it. What
//! holds the panel dark is the open fd, not the mastership.
//!
//! **The re-light is `close(fd)`, and that is the whole of it.** The kernel's last-close handling
//! restores the console and the panel comes back lit (measured: `enabled=enabled dpms=On` within
//! 2 s of the close). There is no saved mode to replay and no restore that can half-fail — which
//! also means **crash safety comes free**, the same property [`crate::panel_dpms`] gets from DPMS
//! being non-persistent: a host that dies holding this has its fds closed by the kernel, and the
//! box lights up. Nothing to journal, nothing to sweep at startup. (Contrast the Windows
//! `pnp_disable_monitors` path, which needs a recovery journal precisely because its disable
//! survives everything.)
//!
//! Best-effort throughout, like every other arm of this policy: a box with no `/dev/dri` at all, a
//! card whose master is held by someone else, or a card with nothing lit simply contributes
//! nothing and the stream proceeds.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

// ---------------------------------------------------------------- the kernel ABI
//
// `include/uapi/drm/drm.h` and `drm_mode.h`. Hand-declared rather than pulled from a crate: this is
// four ioctls and three plain-old-data structs, and the const asserts below pin every layout that
// could drift. `_IO('d', nr)` / `_IOWR('d', nr, T)` encoded by hand — the sizes are in the names.

/// `DRM_IOCTL_SET_MASTER` — `_IO('d', 0x1e)`.
const DRM_IOCTL_SET_MASTER: libc::c_ulong = 0x641e;
/// `DRM_IOCTL_DROP_MASTER` — `_IO('d', 0x1f)`.
const DRM_IOCTL_DROP_MASTER: libc::c_ulong = 0x641f;
/// `DRM_IOCTL_MODE_GETRESOURCES` — `_IOWR('d', 0xA0, drm_mode_card_res)`, 64-byte payload.
const DRM_IOCTL_MODE_GETRESOURCES: libc::c_ulong = 0xC040_64A0;
/// `DRM_IOCTL_MODE_GETCRTC` — `_IOWR('d', 0xA1, drm_mode_crtc)`, 104-byte payload.
const DRM_IOCTL_MODE_GETCRTC: libc::c_ulong = 0xC068_64A1;
/// `DRM_IOCTL_MODE_SETCRTC` — `_IOWR('d', 0xA2, drm_mode_crtc)`, 104-byte payload.
const DRM_IOCTL_MODE_SETCRTC: libc::c_ulong = 0xC068_64A2;

#[repr(C)]
#[derive(Default)]
struct DrmModeCardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmModeModeinfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    type_: u32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmModeCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: DrmModeModeinfo,
}

// The ioctl numbers above encode their payload size (0x40 = 64, 0x68 = 104). If a struct here ever
// disagrees with that, the kernel reads or writes the wrong number of bytes — so pin it at compile
// time rather than discovering it as a corrupted modeset on someone's TV.
const _: () = assert!(std::mem::size_of::<DrmModeCardRes>() == 0x40);
const _: () = assert!(std::mem::size_of::<DrmModeModeinfo>() == 68);
const _: () = assert!(std::mem::size_of::<DrmModeCrtc>() == 0x68);

impl Default for DrmModeCrtc {
    fn default() -> Self {
        // SAFETY: both structs are `repr(C)` plain old data — integers and a `[u8; 32]`, no
        // padding invariants, no pointers that must be valid, and no `Drop`. An all-zero value is
        // a legal instance, and is exactly what the ioctls want for "no connectors, no mode".
        unsafe { std::mem::zeroed() }
    }
}

/// One card we have darkened: the open fd is the hold. Dropping this closes it, and the kernel
/// re-lights — see the module docs.
pub struct DrmDarken {
    /// Kept solely for its `Drop`. The panel stays dark exactly as long as these are open.
    _cards: Vec<File>,
    /// Which `/dev/dri/cardN` we actually turned something off on — logging only.
    pub darkened: Vec<String>,
}

/// `ioctl(fd, req, &mut arg)` for the modeset structs, returning the raw `errno` on failure.
///
/// Split out so each call site is one line and there is exactly one `unsafe` block to justify
/// instead of five near-identical ones.
fn ioctl<T>(fd: libc::c_int, req: libc::c_ulong, arg: &mut T) -> std::io::Result<()> {
    // SAFETY: `fd` is an open DRM node owned by the caller for the whole call; `req` is one of the
    // five `_IO`/`_IOWR` codes declared above, each paired with the `T` its size field names (the
    // const asserts pin that); and `arg` is a live, uniquely-borrowed, `repr(C)` value of that
    // exact type, so the kernel's read/write of `size_of::<T>()` bytes stays inside it.
    let rc = unsafe { libc::ioctl(fd, req, arg as *mut T) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Turn every lit CRTC on every DRM card off, and hold them off. `None` when nothing was darkened
/// — no cards, none masterable, or none lit — and therefore nothing to restore.
pub fn darken() -> Option<DrmDarken> {
    let mut cards = Vec::new();
    let mut darkened = Vec::new();
    for entry in std::fs::read_dir("/dev/dri").ok()?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // `cardN` only: `renderD*` is the render node (no modesetting at all) and `by-path/` is a
        // directory of symlinks to the same nodes.
        if !name.starts_with("card") {
            continue;
        }
        match darken_card(&path) {
            // Masterable, but nothing on this card was lit. Its fd is dropped here, which is
            // correct: we changed nothing, so there is nothing to hold.
            Ok((_, 0)) => {}
            Ok((card, n)) => {
                tracing::debug!(card = name, crtcs = n, "DRM: CRTCs off");
                darkened.push(name.to_string());
                // ⚠ HOLD THE FD THAT DID THE WORK. Closing it and re-opening does not survive the
                // round trip: the close is the kernel's LAST close on that device, which restores
                // the console and re-lights the panel — the fresh fd then holds nothing. Measured
                // on the Nobara VM 2026-08-24, where exactly that shape reported `darkened
                // cards: ["card0"]` while the connector sat at `enabled=enabled dpms=On`.
                cards.push(card);
            }
            Err(why) => tracing::debug!(card = name, %why, "DRM: not ours to darken"),
        }
    }
    if darkened.is_empty() {
        return None;
    }
    Some(DrmDarken {
        _cards: cards,
        darkened,
    })
}

/// Darken one card, returning the open fd **and** how many CRTCs were actually turned off.
///
/// The fd comes back with the count because the caller MUST keep this exact one to hold the panel
/// dark: closing it is the kernel's last close on the device, which restores the console. A card
/// that reports 0 can have its fd dropped freely — nothing was changed to undo.
fn darken_card(path: &Path) -> std::io::Result<(File, usize)> {
    let card = File::options().read(true).write(true).open(path)?;
    let fd = card.as_raw_fd();
    // Someone else driving this card (a live compositor, a foreign gamescope) ⇒ not ours. This is
    // also what keeps the Attach route honest without needing to know about it here.
    ioctl(fd, DRM_IOCTL_SET_MASTER, &mut 0u64)?;

    // Count pass: every pointer NULL, the kernel fills in the counts.
    let mut res = DrmModeCardRes::default();
    ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res)?;
    let n = res.count_crtcs as usize;
    if n == 0 {
        let _ = ioctl(fd, DRM_IOCTL_DROP_MASTER, &mut 0u64);
        return Ok((card, 0));
    }
    // Data pass: hand back a buffer sized by that count and ask again.
    let mut ids = vec![0u32; n];
    let mut res = DrmModeCardRes {
        crtc_id_ptr: ids.as_mut_ptr() as u64,
        count_crtcs: n as u32,
        ..Default::default()
    };
    ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res)?;
    // The kernel may report FEWER than the count pass promised (a hotplug between the two); it
    // never reports more than the buffer we sized, so trust the second count.
    ids.truncate(res.count_crtcs as usize);

    let mut off = 0usize;
    for id in ids {
        let mut crtc = DrmModeCrtc {
            crtc_id: id,
            ..Default::default()
        };
        if ioctl(fd, DRM_IOCTL_MODE_GETCRTC, &mut crtc).is_err() {
            continue;
        }
        // Only touch a CRTC that is actually driving a display. Disabling an already-dark one is a
        // harmless no-op, but counting it would make the log claim a panel went off that never was
        // on — and that verdict is the whole point of reporting a count at all.
        if crtc.mode_valid == 0 && crtc.fb_id == 0 {
            continue;
        }
        // The modeset to "off": no framebuffer, no mode, no connectors.
        let mut disable = DrmModeCrtc {
            crtc_id: id,
            ..Default::default()
        };
        if ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &mut disable).is_ok() {
            off += 1;
        }
    }
    // Hand mastering back immediately: the darkness does not depend on holding it (measured), and
    // the box's own gamescope needs to be able to take this card when the restore relaunches its
    // session. Keeping it would turn a dark panel into a session that cannot start.
    let _ = ioctl(fd, DRM_IOCTL_DROP_MASTER, &mut 0u64);
    Ok((card, off))
}

#[cfg(test)]
mod tests {
    use super::{DrmModeCardRes, DrmModeCrtc, DrmModeModeinfo};

    /// The layouts the ioctl numbers encode. The `const` asserts above already fail the BUILD on
    /// drift; this restates them as a test so the reason is greppable from a failure, and pins the
    /// two field offsets the count/data-pass dance actually depends on.
    #[test]
    fn the_abi_structs_match_the_ioctl_payload_sizes() {
        assert_eq!(std::mem::size_of::<DrmModeCardRes>(), 0x40, "_IOWR 0x40");
        assert_eq!(std::mem::size_of::<DrmModeModeinfo>(), 68);
        assert_eq!(std::mem::size_of::<DrmModeCrtc>(), 0x68, "_IOWR 0x68");
        // `crtc_id_ptr` is the second u64 — the field the data pass points at its id buffer. A
        // reorder here would hand the kernel the framebuffer-id pointer instead.
        assert_eq!(std::mem::offset_of!(DrmModeCardRes, crtc_id_ptr), 8);
        assert_eq!(std::mem::offset_of!(DrmModeCardRes, count_crtcs), 36);
        // `mode` must sit right after the seven u32s, or SETCRTC reads a mode we never wrote.
        assert_eq!(std::mem::offset_of!(DrmModeCrtc, mode), 36);
    }

    /// ON GLASS. Darken this box's panels for real and read the verdict back out of sysfs.
    ///
    /// Run it on a box with a **connected head and no compositor holding the card** — i.e. exactly
    /// the takeover state this module exists for. On the Nobara VM:
    ///
    /// ```sh
    /// # idle the box's gaming session first (what stop_autologin_sessions does), then:
    /// ./pf_vdisplay-<hash> --ignored --nocapture drm_dpms
    /// ```
    ///
    /// Skips itself (rather than failing) when nothing was ours to darken, because that is the
    /// honest outcome on a dev box with a live desktop — the card is already mastered.
    #[test]
    #[ignore = "on glass: needs a connected head and no compositor holding /dev/dri/card*"]
    fn live_the_panels_go_dark_and_come_back() {
        fn connectors() -> Vec<(String, String, String)> {
            let mut v = Vec::new();
            let Ok(rd) = std::fs::read_dir("/sys/class/drm") else {
                return v;
            };
            for e in rd.flatten() {
                let p = e.path();
                let rd = |f: &str| {
                    std::fs::read_to_string(p.join(f))
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default()
                };
                if rd("status") == "connected" {
                    v.push((
                        e.file_name().to_string_lossy().into_owned(),
                        rd("enabled"),
                        rd("dpms"),
                    ));
                }
            }
            v.sort();
            v
        }

        let before = connectors();
        println!("before: {before:?}");
        assert!(
            !before.is_empty(),
            "no connected head — this test needs one to mean anything"
        );

        let Some(hold) = super::darken() else {
            println!("nothing was ours to darken (card already mastered?) — skipping");
            return;
        };
        println!("darkened cards: {:?}", hold.darkened);
        std::thread::sleep(std::time::Duration::from_secs(2));
        let during = connectors();
        println!("during: {during:?}");

        drop(hold);
        std::thread::sleep(std::time::Duration::from_secs(2));
        let after = connectors();
        println!("after:  {after:?}");

        // The claim: every head that was lit went dark, and every one of them came back.
        for (name, en, dpms) in &during {
            assert_eq!(dpms, "Off", "{name} should be DPMS-off while held ({en})");
        }
        assert_eq!(
            after, before,
            "dropping the hold must restore exactly the state we found"
        );
    }

    /// A zeroed `DrmModeCrtc` IS the disable request — that is the only thing `Default` is for
    /// here, so a change that made it non-zero would silently stop disabling anything.
    #[test]
    fn the_default_crtc_is_the_disable_request() {
        let c = DrmModeCrtc::default();
        assert_eq!(c.fb_id, 0, "a framebuffer would keep the CRTC lit");
        assert_eq!(
            c.mode_valid, 0,
            "a valid mode would re-modeset, not disable"
        );
        assert_eq!(c.count_connectors, 0);
        assert_eq!(c.set_connectors_ptr, 0);
    }
}
