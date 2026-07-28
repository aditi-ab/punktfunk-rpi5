//! Ask a devnode which process is serving it — the host half of
//! [`pf_driver_proto::gamepad::ChannelProof`].
//!
//! This is what the pad channel trusts instead of the bootstrap mailbox's `driver_pid`. The mailbox
//! has to be openable by LocalService (that is what the driver's own WUDFHost runs as), and the
//! delivery gate `verify_is_wudfhost` only checks that the named process's IMAGE is the system
//! `WUDFHost.exe` — which is world-executable. So through gamepad proto v2 any LocalService
//! principal, notably the deliberately de-privileged plugin runner, could spawn its own WUDFHost,
//! publish that pid, and be handed the pad's live DATA section: forged HID input into the interactive
//! desktop and a read of the remote user's controller state (security-review 2026-07-28).
//!
//! No host-side check could tell the two apart. Everything the real driver can read at LocalService —
//! the devnode's Location, its `Device Parameters` key, the object namespace — an attacker at
//! LocalService can read too, and both are session-0 LocalService processes running the same image,
//! so token, session and image checks all discriminate nothing. The one thing an attacker cannot
//! forge is **being the driver bound to the devnode we created**: I/O sent to that device reaches
//! only the driver PnP bound to it, and we look the device up by the instance id `SwDeviceCreate`
//! handed back, so a planted look-alike devnode is not in the running either.
//!
//! Three transports, one per driver shape — see [`ProofTransport`] for what each of them cost
//! and why the obvious ones did not survive contact with hidclass.

use anyhow::{anyhow, bail, Context, Result};
use pf_driver_proto::gamepad::{
    ChannelProof, DECK_PROOF_CMD, HID_FEATURE_REPORT_CHANNEL_PROOF, HID_STRING_INDEX_CHANNEL_PROOF,
    IOCTL_PF_GET_CHANNEL_PROOF,
};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Child, CM_Get_Device_IDW, CM_Get_Device_Interface_ListW,
    CM_Get_Device_Interface_List_SizeW, CM_Get_Sibling, CM_Locate_DevNodeW,
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_LOCATE_DEVNODE_NORMAL, CR_SUCCESS,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_GetFeature, HidD_GetIndexedString, HidD_GetProductString, HidD_GetSerialNumberString,
    HidD_SetFeature, GUID_DEVINTERFACE_HID,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

/// `GUID_DEVINTERFACE_XUSB` {EC87F1E3-C13B-4100-B5F7-8B84D54260CB} — the interface `pf_xusb`
/// registers on its own devnode (and what `xinput1_4` enumerates).
const GUID_DEVINTERFACE_XUSB: GUID = GUID::from_u128(0xEC87F1E3_C13B_4100_B5F7_8B84D54260CB);

/// How a driver answers the proof — one variant per driver shape, because what Windows actually
/// carries to each shape differs (all three measured on .173 / Win11 26200, 2026-07-28).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofTransport {
    /// `pf_xusb`: a private IOCTL on `GUID_DEVINTERFACE_XUSB`, the interface it registers on its own
    /// devnode. Works because it is NOT a HID minidriver — nothing sits above it, so it owns both
    /// `IRP_MJ_CREATE` and its own IOCTL dispatch.
    XusbIoctl,
    /// `pf_mouse`: the proof arrives as the HID **serial-number string**. The one transport that
    /// survived measurement against a UMDF HID minidriver — `HidD_GetSerialNumberString` on a
    /// zero-access handle, verified end to end (`PFCP:3:0:7296`, and 7296 was a real
    /// service-spawned `WUDFHost.exe`). Safe here because nothing reads the virtual mouse's serial.
    HidSerialString,
    /// `pf_gamepad` (DualSense / DualShock 4 / Edge / Deck): a HID **feature report**.
    ///
    /// The pads cannot use the serial string — it is what SDL and Steam dedup controllers on, and
    /// Steam is known to mangle a pad's displayed name over serial FORMAT alone. They get a feature
    /// report instead, and it costs NO report-descriptor change, because the captured descriptors
    /// already declare far more feature ids than the driver ever served: `0x85` is declared as a
    /// Feature report on DualSense, DualShock 4 and Edge alike and used to fail with
    /// `STATUS_INVALID_PARAMETER`. The Deck declares one UNNUMBERED feature report driven as
    /// command→response, so its proof rides that existing contract via a private two-byte command.
    HidFeatureReport,
}

/// Ask the devnode `instance_id` (as `SwDeviceCreate` reported it) which process serves it, and
/// return that pid — already checked against `expect_pad_index` and this build's protocol version.
///
/// Every failure is a refusal to deliver, so the errors say exactly which step failed: an operator
/// staring at a dead gamepad needs to know whether the devnode is missing, the interface has not
/// appeared yet, or a driver answered something we did not mint.
pub(super) fn query(
    instance_id: &str,
    transport: ProofTransport,
    expect_pad_index: u32,
) -> Result<u32> {
    let paths = match transport {
        ProofTransport::XusbIoctl => interface_paths(&GUID_DEVINTERFACE_XUSB, instance_id)
            .with_context(|| format!("enumerate the XUSB interface on {instance_id}"))?,
        ProofTransport::HidSerialString | ProofTransport::HidFeatureReport => {
            // hidclass publishes the collection interface on a CHILD PDO, not on our devnode.
            let children = child_device_ids(instance_id)
                .with_context(|| format!("enumerate the HID children of {instance_id}"))?;
            let mut all = Vec::new();
            for child in &children {
                all.extend(interface_paths(&GUID_DEVINTERFACE_HID, child).unwrap_or_default());
            }
            all
        }
    };
    if paths.is_empty() {
        bail!(
            "no device interface for {instance_id} yet — the driver has not finished starting (or \
             is not bound to this devnode at all)"
        );
    }
    // A devnode can publish more than one collection; ask each and take the first well-formed proof.
    let mut last_err = None;
    for path in &paths {
        let r = match transport {
            ProofTransport::XusbIoctl => ask_ioctl(path, expect_pad_index),
            ProofTransport::HidSerialString => ask_hid_path(path, expect_pad_index),
            ProofTransport::HidFeatureReport => ask_feature_path(path, expect_pad_index),
        };
        match r {
            Ok(pid) => return Ok(pid),
            Err(e) => last_err = Some(e.context(format!("ask {path}"))),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no interface answered a channel proof")))
}

/// Open `path` and put the proof IOCTL to it (the private pad-control interface, and `pf_xusb`'s own).
fn ask_ioctl(path: &str, expect_pad_index: u32) -> Result<u32> {
    let handle = open_device(path)?;
    let proof = ask_xusb(HANDLE(handle.as_raw_handle()))?;
    proof
        .check(expect_pad_index)
        .map_err(|why| anyhow!("{why}"))
}

/// Open a HID collection and read the proof out of a FEATURE report — the pad transport.
fn ask_feature_path(path: &str, expect_pad_index: u32) -> Result<u32> {
    let handle = open_device(path)?;
    let proof = ask_feature(HANDLE(handle.as_raw_handle()))?;
    proof
        .check(expect_pad_index)
        .map_err(|why| anyhow!("{why}"))
}

/// The PS identities answer on the declared-but-unserved report `0x85`; the Deck answers its
/// unnumbered report after a private SET_FEATURE command. Both are tried — one driver binary serves
/// four identities and the host does not know which one this devnode became until the DATA section
/// is attached, which is precisely what we are trying to earn the right to do.
fn ask_feature(h: HANDLE) -> Result<ChannelProof> {
    // Feature buffers are sized by the descriptor; the largest of these reports is 64 bytes, and
    // Windows accepts a buffer at least that big.
    const BUF: usize = 64;

    // PS identities: GET_FEATURE 0x85.
    let mut buf = [0u8; BUF];
    buf[0] = HID_FEATURE_REPORT_CHANNEL_PROOF;
    // SAFETY: `h` is the live HID interface handle; `buf` is a valid BUF-sized in/out buffer.
    if unsafe { HidD_GetFeature(h, buf.as_mut_ptr().cast(), BUF as u32) } {
        if let Some(p) = ChannelProof::from_feature_report(&buf) {
            return Ok(p);
        }
    }

    // Deck: SET the private command, then GET the reply. Byte 0 is the (unnumbered) report id 0.
    let mut cmd = [0u8; BUF];
    cmd[1..1 + DECK_PROOF_CMD.len()].copy_from_slice(&DECK_PROOF_CMD);
    // SAFETY: `h` is live; `cmd` is a valid BUF-sized buffer.
    let set_ok = unsafe { HidD_SetFeature(h, cmd.as_mut_ptr().cast(), BUF as u32) };
    if set_ok {
        let mut reply = [0u8; BUF];
        // SAFETY: as above.
        if unsafe { HidD_GetFeature(h, reply.as_mut_ptr().cast(), BUF as u32) }
            && reply.starts_with(&DECK_PROOF_CMD)
        {
            if let Some(p) = ChannelProof::from_bytes(&reply[DECK_PROOF_CMD.len()..]) {
                return Ok(p);
            }
        }
    }
    bail!(
        "this HID collection carries no channel proof (feature 0x{:02x}: no; Deck command: {}) — \
         the driver predates the proof (reinstall: punktfunk-host.exe driver install --gamepad)",
        HID_FEATURE_REPORT_CHANNEL_PROOF,
        if set_ok {
            "no matching reply"
        } else {
            "SET_FEATURE failed"
        },
    )
}

/// Open a HID collection and read the proof out of a string.
fn ask_hid_path(path: &str, expect_pad_index: u32) -> Result<u32> {
    let handle = open_device(path)?;
    let proof = ask_hid(HANDLE(handle.as_raw_handle()))?;
    proof
        .check(expect_pad_index)
        .map_err(|why| anyhow!("{why}"))
}

/// [`query`] for the `channel-proof-probe` subcommand — same call the delivery path makes, so the
/// probe reports the production answer rather than a re-implementation of it.
pub fn probe_pid(
    instance_id: &str,
    transport: ProofTransport,
    expect_pad_index: u32,
) -> Result<u32> {
    query(instance_id, transport, expect_pad_index)
}

/// A human-readable walk of the same lookup [`query`] does, reporting each step and — for the HID
/// leg — which of the two IOCTLs Windows actually forwarded to the minidriver.
///
/// This exists because exactly one thing in this design could not be settled by reading: whether
/// hidclass forwards `IOCTL_HID_GET_INDEXED_STRING` (and/or an arbitrary-index `IOCTL_HID_GET_STRING`)
/// down to a UMDF HID minidriver. Both are wired up so the answer only has to be "at least one", but
/// a maintainer should be able to find out which on a real box in one command rather than by
/// inference — hence `punktfunk-host channel-proof-probe`.
pub fn diagnose(instance_id: &str, transport: ProofTransport, expect_pad_index: u32) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "devnode : {instance_id}");
    let _ = writeln!(
        out,
        "transport: {transport:?}, expected pad index {expect_pad_index}"
    );

    let paths = match transport {
        ProofTransport::XusbIoctl => match interface_paths(&GUID_DEVINTERFACE_XUSB, instance_id) {
            Ok(p) => p,
            Err(e) => {
                let _ = writeln!(out, "  XUSB interface lookup FAILED: {e:#}");
                return out;
            }
        },
        ProofTransport::HidSerialString | ProofTransport::HidFeatureReport => {
            let children = child_device_ids(instance_id).unwrap_or_default();
            let _ = writeln!(out, "  hidclass children: {}", children.len());
            let mut all = Vec::new();
            for c in &children {
                let p = interface_paths(&GUID_DEVINTERFACE_HID, c).unwrap_or_default();
                let _ = writeln!(out, "    {c} -> {} HID interface(s)", p.len());
                all.extend(p);
            }
            all
        }
    };
    if paths.is_empty() {
        let _ = writeln!(
            out,
            "  NO device interface — the driver has not started, or is not bound to this devnode"
        );
        return out;
    }
    for path in &paths {
        let _ = writeln!(out, "  interface {path}");
        let handle = match open_device(path) {
            Ok(h) => h,
            Err(e) => {
                let _ = writeln!(out, "    open FAILED: {e:#}");
                continue;
            }
        };
        let h = HANDLE(handle.as_raw_handle());
        match transport {
            ProofTransport::XusbIoctl => match ask_xusb(h) {
                Ok(p) => {
                    let _ = writeln!(out, "    IOCTL_PF_GET_CHANNEL_PROOF -> {p:?}");
                    let _ = writeln!(out, "    check: {:?}", p.check(expect_pad_index));
                }
                Err(e) => {
                    let _ = writeln!(out, "    IOCTL_PF_GET_CHANNEL_PROOF FAILED: {e:#}");
                }
            },
            ProofTransport::HidFeatureReport => match ask_feature(h) {
                Ok(p) => {
                    let _ = writeln!(out, "    feature proof -> {p:?}");
                    let _ = writeln!(out, "    check: {:?}", p.check(expect_pad_index));
                }
                Err(e) => {
                    let _ = writeln!(out, "    feature proof FAILED: {e:#}");
                }
            },
            ProofTransport::HidSerialString => {
                // Report each path separately — this is the whole point of the probe.
                let (indexed, serial, control) = ask_hid_both(h);
                let _ = writeln!(out, "    HidD_GetSerialNumberString   -> {serial}");
                let _ = writeln!(out, "    HidD_GetIndexedString(proof) -> {indexed}");
                let _ = writeln!(out, "    HidD_GetProductString        -> {control}");
            }
        }
    }
    out
}

/// The probe's raw material: the proof index alongside a KNOWN-GOOD control, so a failure can be
/// told apart from "user mode cannot reach this driver at all".
///
/// The control is `HidD_GetProductString`, which on-glass succeeds against a UMDF HID minidriver on
/// the very same 0-access handle where `HidD_GetIndexedString` fails for every index — the
/// measurement that established the indexed-string transport is unusable (see [`ask_hid`]).
fn ask_hid_both(h: HANDLE) -> (String, String, String) {
    let text = |ok: bool, buf: &[u16]| -> String {
        if !ok {
            let e = std::io::Error::last_os_error();
            return format!("call failed ({e})");
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    };
    let mut a = [0u16; 128];
    let bytes = (a.len() * 2) as u32;
    // SAFETY: `h` is the live HID interface handle; `a` is a valid `bytes`-sized out-buffer.
    let ok_a = unsafe {
        HidD_GetIndexedString(
            h,
            HID_STRING_INDEX_CHANNEL_PROOF,
            a.as_mut_ptr().cast(),
            bytes,
        )
    };
    let indexed = match (ok_a, decode_proof(&a)) {
        (true, Some(p)) => format!("{p:?}"),
        (true, None) => format!("answered, but not a proof: {:?}", text(true, &a)),
        (false, _) => text(false, &a),
    };
    let mut c = [0u16; 128];
    // SAFETY: as above — live handle, valid out-buffer.
    let ok_c = unsafe { HidD_GetSerialNumberString(h, c.as_mut_ptr().cast(), bytes) };
    let serial = match (ok_c, decode_proof(&c)) {
        (true, Some(p)) => format!("{p:?}"),
        (true, None) => format!("plain serial, no proof: {:?}", text(true, &c)),
        (false, _) => text(false, &c),
    };
    let mut b = [0u16; 128];
    // SAFETY: as above — live handle, valid out-buffer.
    let ok_b = unsafe { HidD_GetProductString(h, b.as_mut_ptr().cast(), bytes) };
    let control = format!(
        "{} (control: proves user mode reaches this driver)",
        text(ok_b, &b)
    );
    (indexed, serial, control)
}

/// `CreateFileW` with **no** access rights: enough to send `FILE_ANY_ACCESS` IOCTLs and to run the
/// `HidD_*` helpers, and the only thing that works on a HID mouse/keyboard collection, which Windows
/// refuses to open for read from user mode.
fn open_device(path: &str) -> Result<OwnedHandle> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 path for the duration of the call; the returned
    // handle is owned solely by the `OwnedHandle` built from it.
    let h = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .with_context(|| format!("CreateFileW({path})"))?
    };
    // SAFETY: `h` is the fresh handle just opened, moved into a single owner that closes it on drop.
    Ok(unsafe { OwnedHandle::from_raw_handle(h.0 as _) })
}

/// `pf_xusb`: a private METHOD_BUFFERED IOCTL returning the 16 proof bytes.
fn ask_xusb(h: HANDLE) -> Result<ChannelProof> {
    let mut buf = [0u8; 16];
    let mut returned = 0u32;
    // SAFETY: `h` is the live interface handle; `buf` is a valid 16-byte out-buffer and `returned`
    // a valid out-param. METHOD_BUFFERED, so the I/O manager copies — no pointer escapes.
    unsafe {
        DeviceIoControl(
            h,
            IOCTL_PF_GET_CHANNEL_PROOF,
            None,
            0,
            Some(buf.as_mut_ptr().cast()),
            buf.len() as u32,
            Some(&mut returned),
            None,
        )
        .context("IOCTL_PF_GET_CHANNEL_PROOF")?;
    }
    ChannelProof::from_bytes(&buf[..returned as usize]).ok_or_else(|| {
        anyhow!("the XUSB interface returned {returned} bytes, too short for a channel proof")
    })
}

/// `pf_gamepad` / `pf_mouse`: the reserved HID string index.
///
/// ⚠️ **ON-HARDWARE FINDING, .173 (Win11 26200), 2026-07-28 — this transport DOES NOT WORK.**
/// Probed against a live `pf_mouse` devnode with the installed driver: on the SAME 0-access handle,
/// `HidD_GetManufacturerString` / `GetProductString` / `GetSerialNumberString` all succeeded and
/// returned the driver's OWN strings (so user mode does reach a UMDF HID minidriver, and hidclass
/// does translate the named string IOCTLs down to its `IOCTL_HID_GET_STRING` handler) — while
/// `HidD_GetIndexedString` failed for EVERY index tried: 1, 2, 4, 0x0E and 0x5046. Indices 2 and
/// 0x0E are ones the driver demonstrably serves through the named wrappers, so this is not our
/// reserved index being out of range: hidclass does not carry an arbitrary indexed-string request to
/// a UMDF HID minidriver at all.
///
/// The call is kept because it costs one failed IOCTL and is the correct thing to ask first if a
/// later Windows starts forwarding it. What was REMOVED here is a second "fallback" that issued
/// `IOCTL_HID_GET_STRING` directly: that IOCTL is METHOD_NEITHER and **kernel-facing** — hidclass
/// sends it DOWN to the minidriver, user mode never sends it up — so it could not have worked, and
/// on-glass it returned `ERROR_INVALID_FUNCTION` for a control index the device definitely serves.
/// Do not re-add it.
///
/// The HID pads/mouse therefore still need a working proof transport; see the module docs.
fn ask_hid(h: HANDLE) -> Result<ChannelProof> {
    let mut buf = [0u16; 128];
    let bytes = (buf.len() * 2) as u32;
    // SAFETY: `h` is the live HID interface handle; `buf` is a valid `bytes`-sized out-buffer.
    if unsafe { HidD_GetSerialNumberString(h, buf.as_mut_ptr().cast(), bytes) } {
        if let Some(p) = decode_proof(&buf) {
            return Ok(p);
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        bail!(
            "this HID collection's serial is {:?}, not a channel proof — an old driver is installed \
             (reinstall: punktfunk-host.exe driver install --gamepad)",
            String::from_utf16_lossy(&buf[..end])
        );
    }
    bail!("HidD_GetSerialNumberString failed on this HID collection")
}

/// Decode a NUL-terminated UTF-16 HID string answer into a proof.
fn decode_proof(buf: &[u16]) -> Option<ChannelProof> {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = String::from_utf16(&buf[..end]).ok()?;
    ChannelProof::from_hid_string(&s)
}

/// Every present device-interface path of `class` on `device_id`.
fn interface_paths(class: &GUID, device_id: &str) -> Result<Vec<String>> {
    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
    let mut len = 0u32;
    // SAFETY: `len` is a valid out-param; `wide` is a NUL-terminated id valid for the call.
    let cr = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &mut len,
            class,
            PCWSTR(wide.as_ptr()),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS || len == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` is `len` UTF-16 units as the size call just reported; same id/class as above.
    let cr = unsafe {
        CM_Get_Device_Interface_ListW(
            class,
            PCWSTR(wide.as_ptr()),
            &mut buf,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS {
        bail!("CM_Get_Device_Interface_ListW failed (CONFIGRET {})", cr.0);
    }
    // A REG_MULTI_SZ-shaped list: NUL-separated, double-NUL terminated.
    Ok(buf
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect())
}

/// The instance ids of `instance_id`'s immediate children — for a HID minidriver, the collection
/// PDOs hidclass created under our devnode.
fn child_device_ids(instance_id: &str) -> Result<Vec<String>> {
    let wide: Vec<u16> = instance_id
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut devinst = 0u32;
    // SAFETY: `devinst` is a valid out-param; `wide` is a NUL-terminated id valid for the call.
    let cr = unsafe {
        CM_Locate_DevNodeW(
            &mut devinst,
            PCWSTR(wide.as_ptr()),
            CM_LOCATE_DEVNODE_NORMAL,
        )
    };
    if cr != CR_SUCCESS {
        bail!(
            "CM_Locate_DevNodeW({instance_id}) failed (CONFIGRET {})",
            cr.0
        );
    }
    let mut out = Vec::new();
    let mut child = 0u32;
    // SAFETY: `devinst` is the devnode just located; `child` is a valid out-param.
    if unsafe { CM_Get_Child(&mut child, devinst, 0) } != CR_SUCCESS {
        return Ok(out); // no children yet — hidclass has not enumerated the collections
    }
    loop {
        let mut buf = [0u16; 512];
        // SAFETY: `child` is a live devnode handle from CM_Get_Child/CM_Get_Sibling; `buf` is a
        // valid out-buffer whose length the binding passes along.
        if unsafe { CM_Get_Device_IDW(child, &mut buf, 0) } == CR_SUCCESS {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            out.push(String::from_utf16_lossy(&buf[..end]));
        }
        let mut next = 0u32;
        // SAFETY: as above — `child` is live, `next` is a valid out-param.
        if unsafe { CM_Get_Sibling(&mut next, child, 0) } != CR_SUCCESS {
            break;
        }
        child = next;
    }
    Ok(out)
}
