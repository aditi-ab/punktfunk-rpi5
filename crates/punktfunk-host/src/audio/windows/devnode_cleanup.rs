//! Uninstall-time removal of every audio device punktfunk minted on this box — the
//! `punktfunk-host driver uninstall --audio` leg the installer's Inno `[UninstallRun]` calls.
//!
//! The field report this exists for: uninstalling punktfunk left "Punktfunk Speakers",
//! "Punktfunk Microphone" and the per-pad "Wireless Controller" endpoints sitting in Windows'
//! Sound settings forever. They are not files and no uninstaller deletes them by walking a
//! payload list — they are DEVNODES this host created at runtime, and they persist exactly
//! because they are designed to ([`minted`](super::minted) and
//! [`pad_endpoint`](super::pad_endpoint) both re-resolve their devnodes across host restarts
//! rather than re-minting them). Persistent across restarts must not mean permanent.
//!
//! What gets swept: every MEDIA-class devnode carrying one of the three durable owner markers
//! this product writes into `Device Parameters`, whatever minted it —
//!
//! * [`pad_endpoint::PAD_INDEX_VALUE`](super::pad_endpoint::PAD_INDEX_VALUE) — the per-pad
//!   DualSense speaker endpoints,
//! * [`minted::ROLE_MARKER`](super::minted::ROLE_MARKER) — the Speakers/Microphone substrate,
//! * [`audio_probe::PROBE_MARKER`](super::audio_probe::PROBE_MARKER) — devtest leftovers, so a
//!   probe run on an operator's box cannot outlive the product either.
//!
//! Marker-matched, never name-matched: our devnodes are instances of VALVE's streaming-audio
//! drivers and are name-identical to Steam's own (the same reason the wiring plan works by
//! recorded id). Steam's devnodes, its driver packages, and a VB-CABLE from the era when we
//! bundled one all carry no marker and are therefore untouchable here — uninstalling punktfunk
//! removes what punktfunk created, and nothing else.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{audio_control, audio_probe, minted, pad_endpoint as pe};
use anyhow::Result;
use windows::Win32::Devices::DeviceAndDriverInstallation::SetupDiEnumDeviceInfo;

/// The `Device Parameters` REG_DWORD each punktfunk-minted devnode family stamps on itself. The
/// VALUE is what differs per family; presence of the NAME is "this one is ours", which is all a
/// sweep needs.
const OWNER_MARKERS: [&str; 3] = [
    pe::PAD_INDEX_VALUE,
    minted::ROLE_MARKER,
    audio_probe::PROBE_MARKER,
];

/// What one sweep removed. `endpoint_records` is counted separately from `devnodes` because the
/// registry half is best-effort by design — see [`delete_endpoint_record`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Removed {
    pub devnodes: usize,
    pub devnode_failures: usize,
    pub endpoint_records: usize,
}

/// Restore the default playback device if we left it parked, then remove every audio devnode
/// this product minted, newest registry record and all.
///
/// Best-effort throughout, like the rest of the (un)install path: a devnode that refuses to go
/// is counted and reported, never fatal — a non-zero exit here would abort the whole uninstaller
/// over a virtual speaker.
pub(crate) fn purge() -> Result<Removed> {
    // FIRST, before the sweep deletes the endpoint the default may still point at. A host that
    // died mid-stream leaves the box's default playback parked on our loopback sink; Windows
    // would re-pick on its own once the device vanishes, but by its own ranking rather than by
    // what the operator had. Putting it back is the difference between "the box works again"
    // and "the box works again, on the device it started with".
    if audio_control::unpark_default_for_uninstall() {
        println!("restored the default playback device this host had parked");
    }

    let mut out = Removed::default();
    for inst in owned_devnodes()? {
        // Resolve the endpoint records BEFORE the devnode goes. An endpoint's MMDevices key is
        // tied to us only through its `{1}.<instance id>` devnode link — once the devnode is
        // removed, nothing left in the store says the record was ever ours, and a sweep that
        // guessed by NAME is exactly the mistake this module refuses to make.
        let records: Vec<(&str, String)> = [
            (pe::MMDEV_RENDER_PATH, pe::find_endpoint_for_devnode(&inst)),
            (
                pe::MMDEV_CAPTURE_PATH,
                pe::find_capture_endpoint_for_devnode(&inst),
            ),
        ]
        .into_iter()
        .filter_map(|(path, found)| Some((path, found.ok().flatten()?)))
        .collect();

        if !remove_devnode(&inst) {
            out.devnode_failures += 1;
            // The device is still there, so its record still belongs to a live endpoint.
            continue;
        }
        out.devnodes += 1;
        for (path, endpoint) in records {
            if delete_endpoint_record(path, &endpoint) {
                out.endpoint_records += 1;
            }
        }
    }
    Ok(out)
}

/// Every MEDIA-class devnode carrying one of [`OWNER_MARKERS`]. Enumerated WITHOUT `DIGCF_PRESENT`
/// (that is what [`pe::media_class_devs`] gives us), so a phantom left by a crashed host is swept
/// too — the same "ghost in Device Manager forever" complaint the pad and vdisplay legs fixed.
fn owned_devnodes() -> Result<Vec<String>> {
    let set = pe::media_class_devs()?;
    let mut out = Vec::new();
    for i in 0.. {
        let mut did = pe::devinfo_data();
        // SAFETY: live set; `did` is a live out-param with cbSize set.
        if unsafe { SetupDiEnumDeviceInfo(set.0, i, &mut did) }.is_err() {
            break; // ERROR_NO_MORE_ITEMS
        }
        let Some(inst) = pe::instance_id(&set, &did) else {
            continue;
        };
        if !is_removable_instance(&inst) {
            continue;
        }
        if OWNER_MARKERS
            .iter()
            .any(|m| pe::read_devparam_dword(&set, &did, m).is_some())
        {
            out.push(inst);
        }
    }
    Ok(out)
}

/// A devnode this sweep is allowed to remove: ROOT-enumerated, i.e. software-created.
///
/// Every devnode we mint comes from `SetupDiCreateDeviceInfoW(… DICD_GENERATE_ID)` on the MEDIA
/// class, which always yields `ROOT\MEDIA\NNNN`. Nothing else can be ours — so if a marker name
/// we own ever collides with a value some vendor writes under a REAL sound card's `Device
/// Parameters`, this guard is what stops an uninstall from taking the user's hardware with it.
fn is_removable_instance(instance_id: &str) -> bool {
    instance_id.to_ascii_uppercase().starts_with("ROOT\\")
}

/// `pnputil /remove-device` — the same teardown `audio-probe cleanup` and the driver legs use.
/// Called by absolute path: an uninstaller must not depend on the invoking shell's `%PATH%`.
fn remove_devnode(instance_id: &str) -> bool {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    match std::process::Command::new(format!(r"{windir}\System32\pnputil.exe"))
        .args(["/remove-device", instance_id])
        .output()
    {
        Ok(o) if o.status.success() => {
            println!("removed audio devnode {instance_id}");
            true
        }
        Ok(o) => {
            eprintln!(
                "warning: pnputil could not remove {instance_id} (status {:?}): {}",
                o.status.code(),
                String::from_utf8_lossy(&o.stderr).trim()
            );
            false
        }
        Err(e) => {
            eprintln!("warning: could not run pnputil for {instance_id}: {e}");
            false
        }
    }
}

/// Delete one endpoint's MMDevices record — the `{guid}` subkey holding its name, its stamped
/// formats and its per-endpoint volume/settings.
///
/// BEST-EFFORT ON PURPOSE, and quiet when it fails. These keys are owned by SYSTEM and grant
/// Administrators read only (the same ACL that forces the stamping path through
/// `grant_system_full_control`), while the uninstaller runs elevated but as a USER — so on a
/// stock box this is denied and the record stays. What stays is inert: with the devnode gone the
/// endpoint is NOTPRESENT, which Sound settings surface only behind "Show Disconnected Devices",
/// and nothing re-animates it without a devnode to link to. Buying that last cosmetic scrap would
/// mean an uninstaller seizing ownership of SYSTEM-owned registry keys — a worse thing to ship
/// than the leftover. The DEVICE, which is what the field report was about, is gone either way.
fn delete_endpoint_record(reg_path: &str, endpoint_id: &str) -> bool {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_ALL_ACCESS};
    use winreg::RegKey;

    let Ok(guid) = pe::endpoint_guid_part(endpoint_id) else {
        return false;
    };
    let Ok(store) =
        RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(reg_path, KEY_ALL_ACCESS)
    else {
        return false;
    };
    store.delete_subkey_all(guid).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_enumerated_devnodes_are_ours() {
        assert!(is_removable_instance(r"ROOT\MEDIA\0003"));
        // PnP casing is not guaranteed.
        assert!(is_removable_instance(r"root\media\0004"));
        // A real sound card, however it got a marker-shaped value written under it.
        assert!(!is_removable_instance(
            r"HDAUDIO\FUNC_01&VEN_10EC&DEV_0900\4&1c4a4e5&0&0001"
        ));
        assert!(!is_removable_instance(r"USB\VID_046D&PID_0A38\ABCDEF"));
        // Not a prefix match on the string "ROOT" appearing anywhere.
        assert!(!is_removable_instance(r"SWD\ROOT\MEDIA\0003"));
    }

    #[test]
    fn every_minted_family_is_swept() {
        // The sweep is only as complete as this list — a new minted-devnode family that forgets
        // to register here would ship the same leak again.
        assert!(OWNER_MARKERS.contains(&"PunktfunkPadIndex"));
        assert!(OWNER_MARKERS.contains(&"PunktfunkAudioRole"));
        assert!(OWNER_MARKERS.contains(&"PunktfunkAudioProbe"));
    }
}
