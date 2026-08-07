//! DualSense pad-audio endpoint provisioning (Windows) — mint a per-pad render endpoint that
//! games recognise as the pad's SPEAKER, and loopback-capture what they play into it.
//!
//! Games find a DualSense's audio endpoint by (a) ContainerId — the first render endpoint whose
//! `PKEY_Device_ContainerId` equals the pad's HID container — and/or (b) a FriendlyName
//! containing "Wireless Controller"; the endpoint must be 4 ch / 48 kHz (all verified on-glass
//! 2026-08-01). The punktfunk virtual pad already stamps the deterministic per-pad container
//! `{50464453-0000-0000-0000-00000000000<idx>}` ("PFDS" — see
//! `pf-inject/src/inject/windows/dualsense_windows.rs`, `create_swdevice`), so this module's
//! whole job is to produce a render endpoint carrying the SAME container + name + formats:
//!
//! 1. **Devnode** ([`ensure`], idempotent, host startup — NOT per session): one extra devnode of
//!    Valve's Steam Streaming Speakers driver per pad slot (hwid `ROOT\SteamStreamingSpeakers`,
//!    present whenever Steam is installed; the driver happily multi-instances — each devnode
//!    yields a fresh render endpoint). Registered via `SetupDiRegisterDeviceInfo`, NOT
//!    `SetupDiCallClassInstaller(DIF_REGISTERDEVICE)`, which needs an interactive window
//!    station and fails with error 1459 from a service. The pad slot is persisted in the
//!    devnode's `Device Parameters` key (`PunktfunkPadIndex`) — the INF rewrites DeviceDesc at
//!    driver install, so the marker value is the durable "this one is ours" signal.
//! 2. **Stamp**: the new endpoint gets the MINIMAL DualSense identity — description + device
//!    name, the PFDS container, and the 4 ch/48 kHz format triplet. Never hardware-id or
//!    devicepath properties: an inconsistent stamp makes AudioEndpointBuilder DELETE the
//!    endpoint and re-mint it under a new GUID. The cleaner `IPropertyStore` route is tried
//!    first (audiosrv notices immediately); keys it rejects fall back to raw registry writes
//!    behind the measured MMDevices ACL repair (see [`grant_system_full_control`]).
//! 3. **Capture**: sessions loopback-capture the endpoint ([`PadLoopbackCapturer`], 4 ch f32
//!    interleaved) and ship the PCM to the client's pad speaker/haptics.
//!
//! The wiring plan must never route desktop audio or the virtual mic onto these endpoints —
//! [`audio_control`](super::audio_control) collects the exclusion ids via
//! [`is_pad_render_endpoint`] and the pure plan filters them. A default-playback flip onto a
//! freshly minted pad endpoint is undone inside [`ensure`] (the IPolicyConfig machinery
//! `audio_control` already owns).
//!
//! COM discipline matches the sibling modules: WASAPI/COM objects live on the thread that made
//! them (the provisioning worker, the capture thread); only channels and plain data cross.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{audio_control, AudioCapturer, SAMPLE_RATE};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{HashSet, VecDeque};
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use wasapi::{Direction, SampleType, StreamMode, WaveFormat};
use windows::core::{w, GUID, PCWSTR, PWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiCreateDevRegKeyW, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDevicePropertyW, SetupDiGetDeviceRegistryPropertyW,
    SetupDiOpenDevRegKey, SetupDiRegisterDeviceInfo, SetupDiSetDeviceRegistryPropertyW,
    UpdateDriverForPlugAndPlayDevicesW, DICD_GENERATE_ID, DICS_FLAG_GLOBAL, DIREG_DEV,
    GUID_DEVCLASS_MEDIA, HDEVINFO, SETUP_DI_GET_CLASS_DEVS_FLAGS, SPDRP_HARDWAREID,
    SP_DEVINFO_DATA, UPDATEDRIVERFORPLUGANDPLAYDEVICES_FLAGS,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_DriverInfPath, DEVPROPTYPE, DEVPROP_TYPE_STRING,
};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantClear, PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{CoCreateInstance, BLOB, CLSCTX_ALL, STGM_READ, STGM_READWRITE};
use windows::Win32::System::Registry::{
    RegCloseKey, RegQueryValueExW, RegSetValueExW, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD,
    REG_VALUE_TYPE,
};
use windows::Win32::System::Variant::{VT_BLOB, VT_CLSID, VT_LPWSTR};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

/// Data1 of the deterministic per-pad container GUID ("PFDS") — MUST equal the
/// `container_tag` pf-inject stamps on the virtual DualSense devnodes, or games will never
/// match the endpoint to the pad.
pub(crate) const PFDS_TAG: u32 = 0x5046_4453;
/// DeviceDesc our devnodes are created with (the INF overwrites it at driver install, so this
/// only identifies a devnode whose install never completed — [`PAD_INDEX_VALUE`] is the
/// durable marker).
const DEVNODE_DESC: &str = "Punktfunk Pad Audio";
/// The multi-instancing Steam Remote Play render driver we ride on.
const SSS_HWID: &str = "ROOT\\SteamStreamingSpeakers";
/// Registry value under the devnode's `Device Parameters` key persisting which pad slot the
/// devnode serves (REG_DWORD).
const PAD_INDEX_VALUE: &str = "PunktfunkPadIndex";
/// The endpoint store for render endpoints (each subkey = one endpoint GUID).
const MMDEV_RENDER_PATH: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render";
/// The capture-direction sibling of [`MMDEV_RENDER_PATH`] — where a paired device's microphone
/// half registers (the `audio-probe` devtest's S3 lookup).
const MMDEV_CAPTURE_PATH: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture";
/// WASAPI endpoint-id prefix for render endpoints (`{0.0.0.00000000}.{guid}`).
const ENDPOINT_ID_PREFIX: &str = "{0.0.0.00000000}.";
/// …and for CAPTURE endpoints, whose ids carry `{0.0.1.…}` (measured: the enumeration returns
/// this form, and an id built with the render prefix never string-matches it — the minted
/// mic's capture side resolved to nothing until this was split).
const CAPTURE_ENDPOINT_ID_PREFIX: &str = "{0.0.1.00000000}.";
/// How long [`ensure`] waits for the new render endpoint to materialise after driver install.
const ENDPOINT_WAIT: Duration = Duration::from_secs(10);
/// How many times [`ensure`] re-stamps before giving up and asking for an AudioEndpointBuilder
/// kick (AEB reverts the format keys behind a fresh endpoint — see the loop in [`ensure`]).
const STAMP_ATTEMPTS: usize = 5;
/// How long to let AudioEndpointBuilder settle after a stamp BEFORE checking whether it held.
/// Checking immediately always reports success, including on the passes that get reverted.
const STAMP_SETTLE: Duration = Duration::from_millis(1200);

/// One provisioned pad-audio endpoint. Persistent by design (endpoints survive host restarts);
/// [`remove`] exists for tests + the `pad-endpoint remove` escape hatch only.
#[derive(Debug, Clone)]
pub struct PadEndpoint {
    /// Full WASAPI endpoint id (`{0.0.0.00000000}.{guid}`) — what [`PadLoopbackCapturer::open`]
    /// takes. Empty only from [`find`] when the devnode exists but the endpoint never
    /// registered (driver install incomplete).
    pub endpoint_id: String,
    /// PnP device instance id of the devnode (`ROOT\MEDIA\00NN`).
    pub device_instance: String,
    /// The pad slot this endpoint serves — byte 23 of the stamped container GUID.
    pub pad_index: u8,
    /// True when at least one stamp is STORED but not SERVED: the audio stack must restart
    /// (AudioEndpointBuilder + Audiosrv) before games see the DualSense identity. Never acted
    /// on mid-flight — host startup may do ONE restart before any session exists
    /// ([`provision_at_startup`]); everyone else just reads the flag.
    pub needs_aeb_kick: bool,
}

// --- the stamp set -------------------------------------------------------------------------

/// One endpoint property to stamp: the property-store key, the value, a short log label.
/// pub(crate): the minted-audio provider stamps its endpoint names through the same machinery
/// (store-first, registry fallback, served-check) — see [`write_stamps`].
pub(crate) struct Stamp {
    pub(crate) label: &'static str,
    pub(crate) key: PROPERTYKEY,
    pub(crate) value: StampValue,
}

pub(crate) enum StampValue {
    Str(&'static str),
    /// The PFDS container (VT_CLSID / serialized-CLSID registry blob).
    Container(GUID),
    /// A `WAVEFORMATEXTENSIBLE` (VT_BLOB / serialized-blob registry value).
    Format(&'static [u8; 40]),
}

const fn pkey(fmtid: u128, pid: u32) -> PROPERTYKEY {
    PROPERTYKEY {
        fmtid: GUID::from_u128(fmtid),
        pid,
    }
}

/// `PKEY_Device_DeviceDesc` — the "description" half of the endpoint display name.
pub(crate) const PKEY_DEVICE_DESC: PROPERTYKEY = pkey(0xa45c254e_df1c_4efd_8020_67d146a850e0, 2);
/// Endpoint-store "device name" half of the display name.
pub(crate) const PKEY_ENDPOINT_DEVICE_NAME: PROPERTYKEY =
    pkey(0xb3f8fa53_0004_438e_9003_51a46e139bfc, 6);
/// Endpoint-store devnode link: `"{1}.<device instance id>"` — how an endpoint is tied back to
/// the devnode that owns it.
const PKEY_ENDPOINT_DEVNODE: PROPERTYKEY = pkey(0xb3f8fa53_0004_438e_9003_51a46e139bfc, 2);
/// `PKEY_Device_ContainerId` — what games match against the pad's HID container.
const PKEY_CONTAINER_ID: PROPERTYKEY = pkey(0x8c7ed206_3f8a_4827_b3ab_ae9e1faefc6c, 2);
/// `PKEY_AudioEngine_DeviceFormat` (16-bit PCM leg of the format set).
const PKEY_DEVICE_FORMAT: PROPERTYKEY = pkey(0xf19f064d_082c_4e27_bc73_6882a1bb8e4c, 0);
/// Endpoint format pair (float leg) — pids 2 and 3 of the same fmtid.
const PKEY_MIX_FORMAT_2: PROPERTYKEY = pkey(0x3d6e1656_2e50_4c4c_8d85_d0acae3c6c68, 2);
const PKEY_MIX_FORMAT_3: PROPERTYKEY = pkey(0x3d6e1656_2e50_4c4c_8d85_d0acae3c6c68, 3);
/// Host processing format (float leg).
const PKEY_HOST_FORMAT: PROPERTYKEY = pkey(0xe4870e26_3cc5_4cd2_ba46_ca0a9a70ed04, 0);

/// `WAVEFORMATEXTENSIBLE`: 4 ch / 48 kHz / 16-bit PCM, mask 0x33 (FL FR BL BR), PCM subtype.
const WFX_PCM16_4CH_48K: [u8; 40] = [
    0xfe, 0xff, // wFormatTag = WAVE_FORMAT_EXTENSIBLE
    0x04, 0x00, // nChannels = 4
    0x80, 0xbb, 0x00, 0x00, // nSamplesPerSec = 48000
    0x00, 0xdc, 0x05, 0x00, // nAvgBytesPerSec = 384000
    0x08, 0x00, // nBlockAlign = 8
    0x10, 0x00, // wBitsPerSample = 16
    0x16, 0x00, // cbSize = 22
    0x10, 0x00, // wValidBitsPerSample = 16
    0x33, 0x00, 0x00, 0x00, // dwChannelMask = 0x33
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
    0x71, // KSDATAFORMAT_SUBTYPE_PCM
];
/// `WAVEFORMATEXTENSIBLE`: 4 ch / 48 kHz / 32-bit float, mask 0x33, IEEE-float subtype.
const WFX_F32_4CH_48K: [u8; 40] = [
    0xfe, 0xff, // wFormatTag = WAVE_FORMAT_EXTENSIBLE
    0x04, 0x00, // nChannels = 4
    0x80, 0xbb, 0x00, 0x00, // nSamplesPerSec = 48000
    0x00, 0xb8, 0x0b, 0x00, // nAvgBytesPerSec = 768000
    0x10, 0x00, // nBlockAlign = 16
    0x20, 0x00, // wBitsPerSample = 32
    0x16, 0x00, // cbSize = 22
    0x20, 0x00, // wValidBitsPerSample = 32
    0x33, 0x00, 0x00, 0x00, // dwChannelMask = 0x33
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
    0x71, // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
];

/// The deterministic per-pad container GUID — identical to pf-inject's
/// `GUID::from_values(container_tag, 0, 0, [0,0,0,0,0,0,0,index])` for the DualSense family.
pub(crate) fn pfds_container_guid(pad_index: u8) -> GUID {
    GUID::from_values(PFDS_TAG, 0, 0, [0, 0, 0, 0, 0, 0, 0, pad_index])
}

/// The MINIMAL stamp set — nothing more (hardware-id/devicepath writes make
/// AudioEndpointBuilder delete + re-mint the endpoint under a new GUID).
fn stamps_for(pad_index: u8) -> [Stamp; 7] {
    [
        Stamp {
            label: "device-desc",
            key: PKEY_DEVICE_DESC,
            value: StampValue::Str("Wireless Controller"),
        },
        Stamp {
            label: "device-name",
            key: PKEY_ENDPOINT_DEVICE_NAME,
            value: StampValue::Str("DualSense Wireless Controller"),
        },
        Stamp {
            label: "container-id",
            key: PKEY_CONTAINER_ID,
            value: StampValue::Container(pfds_container_guid(pad_index)),
        },
        Stamp {
            label: "device-format",
            key: PKEY_DEVICE_FORMAT,
            value: StampValue::Format(&WFX_PCM16_4CH_48K),
        },
        Stamp {
            label: "mix-format-2",
            key: PKEY_MIX_FORMAT_2,
            value: StampValue::Format(&WFX_F32_4CH_48K),
        },
        Stamp {
            label: "mix-format-3",
            key: PKEY_MIX_FORMAT_3,
            value: StampValue::Format(&WFX_F32_4CH_48K),
        },
        Stamp {
            label: "host-format",
            key: PKEY_HOST_FORMAT,
            value: StampValue::Format(&WFX_F32_4CH_48K),
        },
    ]
}

/// The stamps to actually apply, honouring the `PUNKTFUNK_PAD_AUDIO_STAMPS` bisect hook.
///
/// Unset (the shipping path) means all seven. Set to a comma-separated list of labels — e.g.
/// `desc,name,container` — and only those are written or checked. This exists because a fully
/// stamped endpoint cannot be opened at all (`IAudioClient::Initialize` → `AUDCLNT_E_UNSUPPORTED
/// _FORMAT` for EVERY format, including its own mix format) while a bare one opens fine, and the
/// MMDevices ACL blocks editing the values directly — so the only way to find the poison stamp is
/// to re-provision with subsets.
fn active_stamps(pad_index: u8) -> Vec<Stamp> {
    let all = stamps_for(pad_index);
    match std::env::var("PUNKTFUNK_PAD_AUDIO_STAMPS") {
        Err(_) => all.into_iter().collect(),
        Ok(list) => {
            let want: HashSet<&str> = list.split(',').map(str::trim).collect();
            all.into_iter().filter(|s| want.contains(s.label)).collect()
        }
    }
}

// --- small encoding helpers ----------------------------------------------------------------

/// NUL-terminated UTF-16.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// REG_MULTI_SZ bytes (UTF-16LE, item NULs + terminating NUL).
fn multi_sz_bytes(items: &[&str]) -> Vec<u8> {
    let mut units: Vec<u16> = items
        .iter()
        .flat_map(|s| s.encode_utf16().chain(std::iter::once(0)))
        .collect();
    units.push(0);
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// REG_SZ bytes (UTF-16LE incl. the NUL).
fn sz_bytes(s: &str) -> Vec<u8> {
    wide(s).iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// Lowercase registry spelling of a GUID (no braces).
fn guid_str(g: &GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}

/// The registry value name MMDevices uses for a property key: `"{fmtid},pid"`.
fn reg_value_name(k: &PROPERTYKEY) -> String {
    format!("{{{}}},{}", guid_str(&k.fmtid), k.pid)
}

/// GUID in registry byte order (Data1/2/3 little-endian, Data4 as-is) — byte 23 of the full
/// serialized container blob ends up being the pad index.
fn guid_registry_bytes(g: &GUID) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&g.data1.to_le_bytes());
    out[4..6].copy_from_slice(&g.data2.to_le_bytes());
    out[6..8].copy_from_slice(&g.data3.to_le_bytes());
    out[8..].copy_from_slice(&g.data4);
    out
}

/// The stamped value in the endpoint store's on-disk shape: strings are plain REG_SZ;
/// container + formats are serialized PROPVARIANTs (8-byte header `vt, 0x00000001`, then the
/// payload) as REG_BINARY.
fn reg_registry_value(v: &StampValue) -> winreg::RegValue<'static> {
    use winreg::enums::{REG_BINARY, REG_SZ};
    match v {
        StampValue::Str(s) => winreg::RegValue {
            bytes: sz_bytes(s).into(),
            vtype: REG_SZ,
        },
        StampValue::Container(g) => {
            let mut b = vec![0x48, 0, 0, 0, 1, 0, 0, 0]; // VT_CLSID header
            b.extend_from_slice(&guid_registry_bytes(g));
            winreg::RegValue {
                bytes: b.into(),
                vtype: REG_BINARY,
            }
        }
        StampValue::Format(wfx) => {
            let mut b = vec![0x41, 0, 0, 0, 1, 0, 0, 0]; // VT_BLOB header
            b.extend_from_slice(&wfx[..]);
            winreg::RegValue {
                bytes: b.into(),
                vtype: REG_BINARY,
            }
        }
    }
}

/// The per-endpoint GUID portion of a WASAPI endpoint id (`{0.0.0.00000000}.{guid}` →
/// `{guid}`) — the endpoint's MMDevices registry key name.
fn endpoint_guid_part(endpoint_id: &str) -> Result<&str> {
    endpoint_id
        .rfind('{')
        .map(|i| &endpoint_id[i..])
        .filter(|g| g.len() >= 38 && g.ends_with('}'))
        .ok_or_else(|| anyhow!("unrecognised endpoint id shape: {endpoint_id}"))
}

// --- PROPVARIANT plumbing -------------------------------------------------------------------
//
// ⚠ `windows 0.62` DOES implement `Drop for PROPVARIANT`, as `PropVariantClear(self)`. Every
// variant below points at memory Rust owns — a `Vec<u16>`, a borrowed `&GUID`, a `&'static
// [u8]` — so letting one drop hands that pointer to `CoTaskMemFree` and corrupts the heap. The
// symptom is not local: provisioning died later with STATUS_HEAP_CORRUPTION (0xC0000374),
// leaving the endpoint half-stamped, which then looked like a stamping-permissions problem.
// Hence `ManuallyDrop`: these variants borrow, so nothing must ever clear them. (Variants that
// come back OWNED from `GetValue` are a different matter and ARE cleared, in `stamp_served`.)

/// A `VT_LPWSTR` variant borrowing `w`, which must outlive it and stay NUL-terminated.
fn pv_lpwstr(w: &[u16]) -> ManuallyDrop<PROPVARIANT> {
    ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_LPWSTR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    pwszVal: PWSTR(w.as_ptr().cast_mut()),
                },
            }),
        },
    })
}

/// A `VT_CLSID` variant borrowing `g`, which must outlive it.
fn pv_clsid(g: &GUID) -> ManuallyDrop<PROPVARIANT> {
    ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_CLSID,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    puuid: std::ptr::from_ref(g).cast_mut(),
                },
            }),
        },
    })
}

/// A `VT_BLOB` variant borrowing `b`, which must outlive it.
fn pv_blob(b: &[u8]) -> ManuallyDrop<PROPVARIANT> {
    ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: b.len() as u32,
                        pBlobData: b.as_ptr().cast_mut(),
                    },
                },
            }),
        },
    })
}

fn pv_string(pv: &PROPVARIANT) -> Option<String> {
    // SAFETY: the variant is initialized (built by us or returned by GetValue); pwszVal is only
    // read when vt says VT_LPWSTR, in which case it points at the variant's NUL-terminated
    // string (or is null, which we check).
    unsafe {
        let inner = &pv.Anonymous.Anonymous;
        if inner.vt != VT_LPWSTR {
            return None;
        }
        let p = inner.Anonymous.pwszVal;
        if p.is_null() {
            return None;
        }
        p.to_string().ok()
    }
}

fn pv_guid(pv: &PROPVARIANT) -> Option<GUID> {
    // SAFETY: as in `pv_string` — puuid is only dereferenced when vt == VT_CLSID and non-null.
    unsafe {
        let inner = &pv.Anonymous.Anonymous;
        if inner.vt != VT_CLSID {
            return None;
        }
        let p = inner.Anonymous.puuid;
        if p.is_null() {
            return None;
        }
        Some(*p)
    }
}

fn pv_bytes(pv: &PROPVARIANT) -> Option<Vec<u8>> {
    // SAFETY: as in `pv_string` — the blob pointer/length pair is only read when vt == VT_BLOB
    // and the pointer is non-null; the variant owns cbSize bytes there.
    unsafe {
        let inner = &pv.Anonymous.Anonymous;
        if inner.vt != VT_BLOB {
            return None;
        }
        let b = &inner.Anonymous.blob;
        if b.pBlobData.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts(b.pBlobData, b.cbSize as usize).to_vec())
    }
}

// --- devnode management (SetupAPI) ----------------------------------------------------------

/// Owns an HDEVINFO and destroys it on drop.
pub(crate) struct DevInfoSet(pub(crate) HDEVINFO);
impl Drop for DevInfoSet {
    fn drop(&mut self) {
        // SAFETY: the handle came from SetupDiGetClassDevsW/SetupDiCreateDeviceInfoList and is
        // destroyed exactly once (this owner's drop).
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

pub(crate) fn media_class_devs() -> Result<DevInfoSet> {
    // SAFETY: the class GUID is a static const; flags 0 (not DIGCF_PRESENT) so a created-but-
    // never-installed phantom from a previous run is still found and reused, not duplicated.
    let set = unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_MEDIA),
            PCWSTR::null(),
            None,
            SETUP_DI_GET_CLASS_DEVS_FLAGS(0),
        )
    }
    .context("SetupDiGetClassDevs(MEDIA)")?;
    Ok(DevInfoSet(set))
}

pub(crate) fn devinfo_data() -> SP_DEVINFO_DATA {
    SP_DEVINFO_DATA {
        cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    }
}

pub(crate) fn instance_id(set: &DevInfoSet, did: &SP_DEVINFO_DATA) -> Option<String> {
    let mut buf = [0u16; 200];
    // SAFETY: live devinfo set + element; the buffer length travels with the slice.
    unsafe { SetupDiGetDeviceInstanceIdW(set.0, did, Some(&mut buf), None) }.ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// A REG_MULTI_SZ SetupDi registry property (e.g. SPDRP_HARDWAREID) as strings.
pub(crate) fn devnode_multi_sz_prop(
    set: &DevInfoSet,
    did: &SP_DEVINFO_DATA,
    prop: windows::Win32::Devices::DeviceAndDriverInstallation::SETUP_DI_REGISTRY_PROPERTY,
) -> Vec<String> {
    let mut buf = vec![0u8; 4096];
    let mut req = 0u32;
    // SAFETY: live set + element; the output buffer length travels with the slice.
    if unsafe {
        SetupDiGetDeviceRegistryPropertyW(set.0, did, prop, None, Some(&mut buf), Some(&mut req))
    }
    .is_err()
    {
        return Vec::new();
    }
    let units: Vec<u16> = buf[..(req as usize).min(buf.len())]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    units
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// The devnode's installed-driver INF filename (`DEVPKEY_Device_DriverInfPath`, e.g.
/// `oem32.inf`) — absent on a devnode whose driver never installed.
pub(crate) fn devnode_inf_path(set: &DevInfoSet, did: &SP_DEVINFO_DATA) -> Option<String> {
    let mut ty = DEVPROPTYPE(0);
    let mut buf = vec![0u8; 1024];
    let mut req = 0u32;
    // SAFETY: live set + element; the property key is a static const; the buffer length
    // travels with the slice.
    unsafe {
        SetupDiGetDevicePropertyW(
            set.0,
            did,
            &DEVPKEY_Device_DriverInfPath,
            &mut ty,
            Some(&mut buf),
            Some(&mut req),
            0,
        )
    }
    .ok()?;
    if ty != DEVPROP_TYPE_STRING {
        return None;
    }
    let units: Vec<u16> = buf[..(req as usize).min(buf.len())]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let len = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    (len > 0).then(|| String::from_utf16_lossy(&units[..len]))
}

/// Read a REG_DWORD from a devnode's `Device Parameters` key — the durable owner-marker
/// mechanism every punktfunk-minted devnode family uses (pad slot, minted-audio role, probe
/// marker). `None`: no key, no value, or wrong type — a foreign devnode.
pub(crate) fn read_devparam_dword(
    set: &DevInfoSet,
    did: &SP_DEVINFO_DATA,
    value_name: &str,
) -> Option<u32> {
    // SAFETY: live set + element; DIREG_DEV opens the devnode's Device Parameters key.
    let hkey = unsafe {
        SetupDiOpenDevRegKey(
            set.0,
            did,
            DICS_FLAG_GLOBAL.0,
            0,
            DIREG_DEV,
            KEY_QUERY_VALUE.0,
        )
    }
    .ok()?;
    let name = wide(value_name);
    let mut data = [0u8; 4];
    let mut len = data.len() as u32;
    let mut ty = REG_VALUE_TYPE(0);
    // SAFETY: the value name is NUL-terminated and outlives the call; data/len are live locals
    // sized together.
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
    (rc.is_ok() && ty == REG_DWORD && len == 4).then(|| u32::from_le_bytes(data))
}

/// Write a REG_DWORD into a devnode's `Device Parameters` key, creating the key on a fresh
/// devnode — the write side of [`read_devparam_dword`].
pub(crate) fn write_devparam_dword(
    set: &DevInfoSet,
    did: &mut SP_DEVINFO_DATA,
    value_name: &str,
    value: u32,
) -> Result<()> {
    // SAFETY: live set + element; DIREG_DEV opens the devnode's Device Parameters key.
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
        // SAFETY: same set + element; a fresh devnode has no Device Parameters key yet, so
        // create it (no INF association).
        Err(_) => unsafe {
            SetupDiCreateDevRegKeyW(
                set.0,
                did,
                DICS_FLAG_GLOBAL.0,
                0,
                DIREG_DEV,
                None,
                PCWSTR::null(),
            )
        }
        .with_context(|| format!("create the Device Parameters key for {value_name}"))?,
    };
    let name = wide(value_name);
    // SAFETY: the value name is NUL-terminated and outlives the call; the DWORD bytes travel
    // with the slice.
    let rc = unsafe {
        RegSetValueExW(
            hkey,
            PCWSTR(name.as_ptr()),
            None,
            REG_DWORD,
            Some(&value.to_le_bytes()),
        )
    };
    // SAFETY: closing the key opened/created above, exactly once.
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    rc.ok().with_context(|| format!("write {value_name}"))
}

/// The persisted pad slot of a devnode (the `PunktfunkPadIndex` value under its
/// `Device Parameters` key), or `None` for foreign devnodes.
fn devnode_pad_index(set: &DevInfoSet, did: &SP_DEVINFO_DATA) -> Option<u32> {
    read_devparam_dword(set, did, PAD_INDEX_VALUE)
}

/// Find the devnode previously created for `pad_index` (see the module doc: the persisted
/// index value is the durable marker; DeviceDesc only survives until the INF installs).
fn find_devnode(pad_index: u8) -> Result<Option<String>> {
    let set = media_class_devs()?;
    for i in 0.. {
        let mut did = devinfo_data();
        // SAFETY: live set; `did` is a live out-param with cbSize set.
        if unsafe { SetupDiEnumDeviceInfo(set.0, i, &mut did) }.is_err() {
            break; // ERROR_NO_MORE_ITEMS
        }
        let Some(inst) = instance_id(&set, &did) else {
            continue;
        };
        if !inst.to_ascii_uppercase().starts_with("ROOT\\") {
            continue;
        }
        if devnode_pad_index(&set, &did) == Some(pad_index as u32) {
            return Ok(Some(inst));
        }
    }
    Ok(None)
}

/// Create + register a fresh MEDIA-class root devnode carrying `hwid`, then let `mark` write
/// the caller's durable owner marker into its `Device Parameters` key (DeviceDesc only
/// survives until the INF installs — see the module doc). Shared by the pad provisioner and
/// the `audio-probe` devtest: the first slice of the shared minting surface the
/// audio-substrate design (`windows-audio-endpoints-and-vbcable.md` §C1) extracts.
pub(crate) fn create_media_devnode(
    desc: &str,
    hwid: &str,
    mark: impl FnOnce(&DevInfoSet, &mut SP_DEVINFO_DATA) -> Result<()>,
) -> Result<String> {
    // SAFETY: the class GUID is a static const.
    let set = unsafe { SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_MEDIA), None) }
        .context("SetupDiCreateDeviceInfoList(MEDIA)")?;
    let set = DevInfoSet(set);
    let mut did = devinfo_data();
    let desc = wide(desc);
    // SAFETY: name/class/description are live NUL-terminated buffers; DICD_GENERATE_ID makes
    // PnP mint the ROOT\MEDIA\00NN instance id; `did` receives the element.
    unsafe {
        SetupDiCreateDeviceInfoW(
            set.0,
            w!("MEDIA"),
            &GUID_DEVCLASS_MEDIA,
            PCWSTR(desc.as_ptr()),
            None,
            DICD_GENERATE_ID,
            Some(&mut did),
        )
    }
    .context("SetupDiCreateDeviceInfo")?;
    let hwid = multi_sz_bytes(&[hwid]);
    // SAFETY: live set + element; the multi-sz property bytes travel with the slice.
    unsafe { SetupDiSetDeviceRegistryPropertyW(set.0, &mut did, SPDRP_HARDWAREID, Some(&hwid)) }
        .context("set SPDRP_HARDWAREID")?;
    // NOT SetupDiCallClassInstaller(DIF_REGISTERDEVICE): that requires an interactive window
    // station and fails with error 1459 from a service. Plain registration is all a root
    // devnode needs before UpdateDriverForPlugAndPlayDevices binds the driver.
    // SAFETY: live set + element; no compare callback.
    unsafe { SetupDiRegisterDeviceInfo(set.0, &mut did, 0, None, None, None) }
        .context("SetupDiRegisterDeviceInfo")?;
    mark(&set, &mut did)?;
    instance_id(&set, &did).context("read the new devnode's instance id")
}

/// Create + register a fresh MEDIA-class root devnode carrying the Steam Streaming Speakers
/// hardware id, and persist the pad slot in its `Device Parameters` key.
fn create_devnode(pad_index: u8) -> Result<String> {
    let inst = create_media_devnode(DEVNODE_DESC, SSS_HWID, |set, did| {
        write_pad_index(set, did, pad_index)
    })?;
    tracing::info!(pad = pad_index, devnode = %inst, "created a pad-audio devnode");
    Ok(inst)
}

/// Persist `pad_index` in the devnode's `Device Parameters` key (created on a fresh devnode).
fn write_pad_index(set: &DevInfoSet, did: &mut SP_DEVINFO_DATA, pad_index: u8) -> Result<()> {
    write_devparam_dword(set, did, PAD_INDEX_VALUE, pad_index as u32)
}

/// The Steam Streaming Speakers INF to feed `UpdateDriverForPlugAndPlayDevices`: prefer the
/// INSTALLED driver's `oemNN.inf` (via `DEVPKEY_Device_DriverInfPath` on any devnode already
/// bound to the SSS hwid — Steam's own SSS devnode, or a pad devnode from an earlier run);
/// fall back to Steam's driver directory when no bound devnode exists yet.
fn resolve_sss_inf() -> Result<String> {
    let set = media_class_devs()?;
    for i in 0.. {
        let mut did = devinfo_data();
        // SAFETY: live set; `did` is a live out-param with cbSize set.
        if unsafe { SetupDiEnumDeviceInfo(set.0, i, &mut did) }.is_err() {
            break;
        }
        if !devnode_multi_sz_prop(&set, &did, SPDRP_HARDWAREID)
            .iter()
            .any(|h| h.eq_ignore_ascii_case(SSS_HWID))
        {
            continue;
        }
        if let Some(inf) = devnode_inf_path(&set, &did) {
            let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
            let full = format!(r"{windir}\INF\{inf}");
            if std::path::Path::new(&full).exists() {
                return Ok(full);
            }
        }
    }
    if let Some(w) = super::wasapi_mic::steam_driver_inf_path("SteamStreamingSpeakers.inf") {
        let s = String::from_utf16_lossy(&w);
        let s = s.trim_end_matches('\0').to_string();
        if std::path::Path::new(&s).exists() {
            return Ok(s);
        }
    }
    bail!(
        "no Steam Streaming Speakers INF found (no installed SSS devnode, and Steam's driver \
         directory is absent) — install Steam, whose Remote Play streaming drivers provide it"
    )
}

/// Bind `inf` to every unbound devnode carrying `hwid`. Idempotent: "nothing needed an
/// update" is success. Shared with the `audio-probe` devtest (§C1 minting surface).
pub(crate) fn bind_driver(hwid: &str, inf: &str) -> Result<()> {
    let inf_w = wide(inf);
    let hwid_w = wide(hwid);
    // SAFETY: both strings are NUL-terminated and outlive the call; a null parent HWND and no
    // reboot-required out-param are documented as accepted.
    let r = unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            None,
            PCWSTR(hwid_w.as_ptr()),
            PCWSTR(inf_w.as_ptr()),
            UPDATEDRIVERFORPLUGANDPLAYDEVICES_FLAGS(0),
            None,
        )
    };
    match r {
        Ok(()) => {
            tracing::info!(hwid = %hwid, inf = %inf, "bound the driver to the unbound devnode(s)");
            Ok(())
        }
        // ERROR_NO_MORE_ITEMS (0x80070103): every matching devnode already runs this (or a
        // better) driver — the idempotent-reissue case, not a failure.
        Err(e) if e.code().0 as u32 == 0x8007_0103 => Ok(()),
        Err(e) => {
            Err(anyhow!(e)).with_context(|| format!("UpdateDriverForPlugAndPlayDevices({inf})"))
        }
    }
}

/// Bind the SSS driver to every unbound devnode carrying its hardware id (i.e. the pad
/// devnodes just created). Idempotent: "nothing needed an update" is success.
fn install_sss_driver() -> Result<()> {
    bind_driver(SSS_HWID, &resolve_sss_inf()?)
}

// --- endpoint discovery + stamping ----------------------------------------------------------

/// The render endpoint owned by `instance_id`, identified through the endpoint store's devnode
/// link (`"{1}.<instance id>"` under `…\MMDevices\Audio\Render\{ep}\Properties`).
pub(crate) fn find_endpoint_for_devnode(instance_id: &str) -> Result<Option<String>> {
    endpoint_for_devnode_in(MMDEV_RENDER_PATH, ENDPOINT_ID_PREFIX, instance_id)
}

/// The CAPTURE endpoint owned by `instance_id` — the microphone half of a paired device like
/// the Steam Streaming Microphone. Pad devices are render-only; the minted-audio provider and
/// the `audio-probe` devtest need this direction.
pub(crate) fn find_capture_endpoint_for_devnode(instance_id: &str) -> Result<Option<String>> {
    endpoint_for_devnode_in(MMDEV_CAPTURE_PATH, CAPTURE_ENDPOINT_ID_PREFIX, instance_id)
}

fn endpoint_for_devnode_in(
    reg_path: &str,
    id_prefix: &str,
    instance_id: &str,
) -> Result<Option<String>> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    let want = format!("{{1}}.{instance_id}");
    let root = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(reg_path)
        .with_context(|| format!(r"open HKLM\{reg_path}"))?;
    for key in root.enum_keys().flatten() {
        let Ok(props) = root.open_subkey(format!(r"{key}\Properties")) else {
            continue;
        };
        let Ok(link) = props.get_value::<String, _>(reg_value_name(&PKEY_ENDPOINT_DEVNODE)) else {
            continue;
        };
        if link.eq_ignore_ascii_case(&want) {
            return Ok(Some(format!("{id_prefix}{key}")));
        }
    }
    Ok(None)
}

/// Poll for the endpoint after driver install (audiosrv registers it asynchronously).
fn wait_for_endpoint(instance_id: &str) -> Result<String> {
    let deadline = Instant::now() + ENDPOINT_WAIT;
    loop {
        if let Some(ep) = find_endpoint_for_devnode(instance_id)? {
            return Ok(ep);
        }
        if Instant::now() >= deadline {
            bail!(
                "no render endpoint appeared for {instance_id} within {}s — is Audiosrv \
                 running?",
                ENDPOINT_WAIT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn open_mmdevice(endpoint_id: &str) -> Result<IMMDevice> {
    let id_w = wide(endpoint_id);
    // SAFETY: standard COM activation on a COM-initialized thread; the id buffer is
    // NUL-terminated and outlives the call.
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .context("CoCreateInstance(MMDeviceEnumerator)")?;
        en.GetDevice(PCWSTR(id_w.as_ptr()))
            .with_context(|| format!("IMMDeviceEnumerator::GetDevice({endpoint_id})"))
    }
}

/// Open a [`wasapi::Device`] for an endpoint id WITHOUT the crate's `DeviceEnumerator::get_device`.
///
/// `wasapi 0.23` builds that call's argument as
/// `PCWSTR::from_raw(HSTRING::from(device_id).as_ptr())`. The `HSTRING` is a temporary, so it is
/// dropped at the end of THAT statement and `IMMDeviceEnumerator::GetDevice` reads freed memory on
/// the next line. Whether the endpoint is found then depends on what the allocator happened to
/// leave behind — a heisenbug whose failure mode is `0x80070002` (ERROR_FILE_NOT_FOUND) for an id
/// that is perfectly valid. [`open_mmdevice`] keeps its wide buffer alive across the call, so
/// resolve there and only borrow the crate's wrapper around the resulting interface.
pub(crate) fn open_wasapi_device(endpoint_id: &str) -> Result<wasapi::Device> {
    let dev = open_mmdevice(endpoint_id)?;
    wasapi::Device::from_immdevice(dev)
        .map_err(|e| anyhow!("wrap IMMDevice {endpoint_id} as a wasapi Device: {e}"))
}

/// Log whether this endpoint can be activated IN THIS PROCESS, at both layers.
///
/// The whole pad-audio bring-up stalled on an `IAudioClient: 0x80070002` that named no layer: the
/// endpoint resolved, the property store opened, and only activation failed — which is equally
/// consistent with a dead endpoint, a process that cannot activate anything, and a bad argument
/// handed to the COM call. Reporting the raw `IMMDevice::Activate` result next to the crate's
/// tells them apart in one run instead of one rebuild each.
fn probe_activation(endpoint_id: &str) {
    match open_mmdevice(endpoint_id) {
        Err(e) => tracing::error!(endpoint = %endpoint_id, error = %format!("{e:#}"),
            "activation probe: GetDevice failed"),
        Ok(dev) => {
            // SAFETY: standard COM activation on a COM-initialized thread; the returned
            // interface is dropped immediately (the probe only wants the HRESULT).
            match unsafe { dev.Activate::<IAudioClient>(CLSCTX_ALL, None) } {
                Ok(_) => tracing::info!(endpoint = %endpoint_id,
                    "activation probe: raw IMMDevice::Activate(IAudioClient) OK"),
                Err(e) => tracing::error!(endpoint = %endpoint_id,
                    hr = %format!("{:#010x}", e.code().0),
                    "activation probe: raw IMMDevice::Activate(IAudioClient) FAILED"),
            }
        }
    }
}

/// Does the property store SERVE this stamp's value right now?
fn stamp_served(store: &IPropertyStore, s: &Stamp) -> bool {
    // SAFETY: the key is a valid PROPERTYKEY; GetValue returns an owned variant that is
    // cleared below, exactly once.
    let Ok(mut pv) = (unsafe { store.GetValue(&s.key) }) else {
        return false;
    };
    let matches = match &s.value {
        StampValue::Str(v) => pv_string(&pv).is_some_and(|got| got == *v),
        StampValue::Container(g) => pv_guid(&pv) == Some(*g),
        StampValue::Format(wfx) => pv_bytes(&pv).is_some_and(|got| got == wfx[..]),
    };
    // SAFETY: `pv` owns store-allocated memory; cleared exactly once, then dropped inert.
    unsafe {
        let _ = PropVariantClear(&mut pv);
    }
    matches
}

fn set_store_value(store: &IPropertyStore, s: &Stamp) -> Result<()> {
    match &s.value {
        StampValue::Str(v) => {
            let buf = wide(v);
            let pv = pv_lpwstr(&buf);
            // SAFETY: the variant borrows `buf`, which outlives the call; SetValue copies.
            unsafe { store.SetValue(&s.key, &*pv) }.context(s.label)?;
        }
        StampValue::Container(g) => {
            let pv = pv_clsid(g);
            // SAFETY: the variant borrows `*g`, which outlives the call; SetValue copies.
            unsafe { store.SetValue(&s.key, &*pv) }.context(s.label)?;
        }
        StampValue::Format(wfx) => {
            let pv = pv_blob(&wfx[..]);
            // SAFETY: the variant borrows the static format bytes; SetValue copies.
            unsafe { store.SetValue(&s.key, &*pv) }.context(s.label)?;
        }
    }
    Ok(())
}

/// Write the still-missing stamps: IPropertyStore first (audiosrv notices immediately, no
/// restart), raw registry for whatever it rejects. Idempotent — already-served keys are
/// skipped entirely.
fn stamp_endpoint(endpoint_id: &str, pad_index: u8) -> Result<()> {
    write_stamps(endpoint_id, &active_stamps(pad_index))
}

/// The generic stamp writer behind [`stamp_endpoint`], shared with the minted-audio provider
/// (which stamps "Punktfunk Speakers/Microphone" names — field-measured necessity: without
/// them even the box's owner could not tell the minted instances from Steam's primaries in
/// the Sound settings zoo).
pub(crate) fn write_stamps(endpoint_id: &str, stamps: &[Stamp]) -> Result<()> {
    let dev = open_mmdevice(endpoint_id)?;
    let pending: Vec<&Stamp> = {
        // SAFETY: read-only property store on a COM-initialized thread.
        let store =
            unsafe { dev.OpenPropertyStore(STGM_READ) }.context("OpenPropertyStore(STGM_READ)")?;
        stamps.iter().filter(|s| !stamp_served(&store, s)).collect()
    };
    if pending.is_empty() {
        tracing::debug!(endpoint = %endpoint_id, "endpoint already fully stamped");
        return Ok(());
    }
    let mut via_store: Vec<&'static str> = Vec::new();
    let mut via_registry: Vec<&Stamp> = Vec::new();
    // SAFETY: read-write property store on a COM-initialized thread (may be denied — handled).
    match unsafe { dev.OpenPropertyStore(STGM_READWRITE) } {
        Ok(rw) => {
            for s in &pending {
                match set_store_value(&rw, s) {
                    Ok(()) => via_store.push(s.label),
                    Err(e) => {
                        tracing::debug!(key = s.label, error = %format!("{e:#}"),
                            "IPropertyStore rejected a pad stamp — registry route");
                        via_registry.push(s);
                    }
                }
            }
            if !via_store.is_empty() {
                // SAFETY: committing the writes above on the same store/thread.
                if let Err(e) = unsafe { rw.Commit() } {
                    // A failed commit may have dropped every store-side write — re-route them
                    // all through the registry rather than trust half a stamp.
                    tracing::debug!(error = %format!("{e:#}"),
                        "IPropertyStore::Commit failed — registry route for all pending stamps");
                    via_store.clear();
                    via_registry = pending.clone();
                }
            }
        }
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"),
                "pad endpoint property store not writable — registry route for all stamps");
            via_registry = pending.clone();
        }
    }
    if !via_registry.is_empty() {
        registry_stamp(endpoint_id, &via_registry)?;
    }
    tracing::info!(
        endpoint = %endpoint_id,
        property_store = ?via_store,
        registry = ?via_registry.iter().map(|s| s.label).collect::<Vec<_>>(),
        "endpoint stamped (route per key)"
    );
    Ok(())
}

/// MEASURED ACL trap (on-glass 2026-08-01): `…\MMDevices\Audio\Render\{ep}\Properties` denies
/// writes even to SYSTEM — SYSTEM is the key OWNER but the DACL carries no write ACE for it.
/// The owner's IMPLICIT right to WRITE_DAC still applies, so: open with
/// READ_CONTROL|WRITE_DAC, append a FullControl ACE for S-1-5-18, write the DACL back.
/// Principals are resolved BY SID (a well-known-SID build), never by name — account-name
/// lookups like "BUILTIN\Administrators" fail to translate on localized (e.g. German) Windows.
/// Works when running as SYSTEM (the production host service); a dev run fails here and the
/// caller degrades with the error.
fn grant_system_full_control(subkey_path: &str) -> Result<()> {
    use windows::Win32::Foundation::{LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{
        GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, EXPLICIT_ACCESS_W, GRANT_ACCESS,
        NO_MULTIPLE_TRUSTEE, SE_REGISTRY_KEY, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows::Win32::Security::{
        CreateWellKnownSid, WinLocalSystemSid, ACL, CONTAINER_INHERIT_ACE,
        DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_MAX_SID_SIZE,
    };
    use windows::Win32::System::Registry::{
        RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_ALL_ACCESS, REG_SAM_FLAGS,
    };

    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    let path_w = wide(subkey_path);
    let mut hkey = HKEY::default();
    // SAFETY: the path is NUL-terminated and outlives the call; hkey is a live out-param.
    unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path_w.as_ptr()),
            None,
            REG_SAM_FLAGS(READ_CONTROL | WRITE_DAC),
            &mut hkey,
        )
    }
    .ok()
    .with_context(|| {
        format!("open {subkey_path} for WRITE_DAC (owner-implicit right — requires SYSTEM)")
    })?;
    let handle = HANDLE(hkey.0);
    let mut old_dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: live handle; out-params are live locals; the returned descriptor is LocalFree'd
    // below.
    let gs = unsafe {
        GetSecurityInfo(
            handle,
            SE_REGISTRY_KEY,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_dacl),
            None,
            Some(&mut sd),
        )
    };
    let result = (|| -> Result<()> {
        gs.ok().context("GetSecurityInfo(DACL)")?;
        let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut cb = sid.len() as u32;
        // SAFETY: the buffer is SECURITY_MAX_SID_SIZE, the documented maximum SID size.
        unsafe {
            CreateWellKnownSid(
                WinLocalSystemSid,
                None,
                Some(PSID(sid.as_mut_ptr().cast())),
                &mut cb,
            )
        }
        .context("CreateWellKnownSid(S-1-5-18)")?;
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: KEY_ALL_ACCESS.0,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: CONTAINER_INHERIT_ACE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: PWSTR(sid.as_mut_ptr().cast()),
            },
        };
        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: one live entry whose SID buffer outlives the call; old_dacl is the (possibly
        // null) DACL GetSecurityInfo returned, still owned by `sd`.
        unsafe {
            SetEntriesInAclW(
                Some(&[ea]),
                (!old_dacl.is_null()).then_some(old_dacl as *const ACL),
                &mut new_dacl,
            )
        }
        .ok()
        .context("SetEntriesInAclW")?;
        // SAFETY: new_dacl is the ACL SetEntriesInAclW just allocated; freed right after.
        let ss = unsafe {
            SetSecurityInfo(
                handle,
                SE_REGISTRY_KEY,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(new_dacl),
                None,
            )
        };
        // SAFETY: LocalFree of the SetEntriesInAclW allocation, exactly once.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(new_dacl.cast())));
        }
        ss.ok().context("SetSecurityInfo(DACL)")
    })();
    // SAFETY: free the descriptor GetSecurityInfo allocated (skipped when null) and close the
    // key opened above, each exactly once.
    unsafe {
        if !sd.0.is_null() {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        let _ = RegCloseKey(hkey);
    }
    result
}

/// The raw-registry stamp route: repair the Properties key ACL, then write the serialized
/// values (see [`reg_registry_value`]). Values written here are STORED but possibly not
/// SERVED until an AudioEndpointBuilder restart — the caller's read-back decides.
fn registry_stamp(endpoint_id: &str, stamps: &[&Stamp]) -> Result<()> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    let guid = endpoint_guid_part(endpoint_id)?;
    let path = format!(r"{MMDEV_RENDER_PATH}\{guid}\Properties");
    grant_system_full_control(&path)
        .with_context(|| format!("make {path} writable (registry stamp route)"))?;
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(&path, KEY_QUERY_VALUE.0 | KEY_SET_VALUE.0)
        .with_context(|| format!("open {path} for writing"))?;
    for s in stamps {
        key.set_raw_value(reg_value_name(&s.key), &reg_registry_value(&s.value))
            .with_context(|| format!("write {} ({})", reg_value_name(&s.key), s.label))?;
    }
    Ok(())
}

/// True when EVERY stamp reads back — through a fresh property store — with the stamped value,
/// i.e. the audio stack SERVES the identity rather than merely storing it. Any error counts as
/// "not served" (the only consumer is the needs-AEB-kick decision).
fn all_served(endpoint_id: &str, pad_index: u8) -> bool {
    stamps_served(endpoint_id, &active_stamps(pad_index))
}

/// [`all_served`]'s generic body — shared with the minted-audio provider.
pub(crate) fn stamps_served(endpoint_id: &str, stamps: &[Stamp]) -> bool {
    let Ok(dev) = open_mmdevice(endpoint_id) else {
        return false;
    };
    // SAFETY: read-only property store on a COM-initialized thread.
    let Ok(store) = (unsafe { dev.OpenPropertyStore(STGM_READ) }) else {
        return false;
    };
    stamps.iter().all(|s| stamp_served(&store, s))
}

// --- public provisioning API ----------------------------------------------------------------

/// Idempotently provision the pad-audio endpoint for one pad slot: reuse (or create) the
/// devnode, bind the Steam Streaming Speakers driver, wait for the endpoint, stamp the
/// DualSense identity, and undo any default-playback flip onto the new endpoint. Called at
/// host startup (pre-provisioning) — NOT per session. Must run on (or become) a
/// COM-initialized thread; WASAPI objects never leave it.
pub fn ensure(pad_index: u8) -> Result<PadEndpoint> {
    anyhow::ensure!(pad_index < 8, "pad index out of range (0..=7): {pad_index}");
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")?;
    let prev_default = audio_control::default_render_id();
    let device_instance = match find_devnode(pad_index)? {
        Some(inst) => inst,
        None => create_devnode(pad_index)?,
    };
    let endpoint_id = match find_endpoint_for_devnode(&device_instance)? {
        Some(ep) => ep,
        None => {
            install_sss_driver().context("bind the Steam Streaming Speakers driver")?;
            wait_for_endpoint(&device_instance)?
        }
    };
    // Stamp until it STAYS stamped. On a freshly created endpoint the write lands — a check run
    // straight afterwards reports all seven served — and AudioEndpointBuilder then quietly reverts
    // the three format keys behind us, leaving 4/7 for good. So the check has to happen after a
    // settle, not immediately: verifying too early is exactly how this looked like "the stamps
    // never took" for one debugging session and "the stamps took fine" for the next.
    //
    // Converging here matters beyond tidiness: `needs_aeb_kick` is what makes
    // [`provision_at_startup`] restart AudioEndpointBuilder + Audiosrv, so latching the transient
    // means bouncing the whole machine's audio stack on EVERY host start, forever, chasing stamps
    // a re-pass would have landed. Once the endpoint has finished settling a re-stamp sticks
    // permanently (measured stable over 30 s), and `stamp_endpoint` skips already-served keys, so
    // the extra passes cost nothing once it has taken.
    let mut served = false;
    for attempt in 0..STAMP_ATTEMPTS {
        stamp_endpoint(&endpoint_id, pad_index)
            .with_context(|| format!("stamp pad endpoint {endpoint_id}"))?;
        thread::sleep(STAMP_SETTLE);
        served = all_served(&endpoint_id, pad_index);
        if served {
            if attempt > 0 {
                tracing::debug!(
                    pad = pad_index,
                    attempt = attempt + 1,
                    "pad endpoint stamps held after a re-pass"
                );
            }
            break;
        }
    }
    // Default-device guard: a freshly registered render endpoint can grab the default. A pad
    // "speaker" as default playback would swallow ALL desktop audio — put the previous default
    // back via the IPolicyConfig machinery audio_control already owns.
    if audio_control::default_render_id().as_deref() == Some(endpoint_id.as_str())
        && prev_default.as_deref() != Some(endpoint_id.as_str())
    {
        match &prev_default {
            Some(prev) => match audio_control::set_default_endpoint(prev) {
                Ok(()) => tracing::info!(pad = pad_index,
                    "default playback had moved to the new pad endpoint — restored the previous default"),
                Err(e) => tracing::warn!(pad = pad_index, error = %format!("{e:#}"),
                    "default playback moved to the new pad endpoint and could not be restored"),
            },
            None => tracing::warn!(pad = pad_index,
                "default playback moved to the new pad endpoint and no previous default is known"),
        }
    }
    Ok(PadEndpoint {
        endpoint_id,
        device_instance,
        pad_index,
        needs_aeb_kick: !served,
    })
}

/// Best-effort teardown by devnode removal (`pnputil /remove-device`). Not called in normal
/// operation — endpoints are persistent by design; this backs tests and the
/// `pad-endpoint remove` escape hatch.
pub fn remove(pe: &PadEndpoint) {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let pnputil = format!(r"{windir}\System32\pnputil.exe");
    match std::process::Command::new(&pnputil)
        .args(["/remove-device", &pe.device_instance])
        .output()
    {
        Ok(o) if o.status.success() => {
            tracing::info!(devnode = %pe.device_instance, "pad-audio devnode removed")
        }
        Ok(o) => tracing::warn!(devnode = %pe.device_instance, status = ?o.status.code(),
            stderr = %String::from_utf8_lossy(&o.stderr).trim(),
            "pnputil could not remove the pad-audio devnode"),
        Err(e) => tracing::warn!(devnode = %pe.device_instance, error = %e,
            "could not run pnputil to remove the pad-audio devnode"),
    }
}

/// Locate (never create) the provisioned state for a pad slot — the `status`/`remove` devtest
/// path. `endpoint_id` is empty when the devnode exists but its endpoint never registered.
pub(crate) fn find(pad_index: u8) -> Result<Option<PadEndpoint>> {
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")?;
    let Some(device_instance) = find_devnode(pad_index)? else {
        return Ok(None);
    };
    let endpoint_id = find_endpoint_for_devnode(&device_instance)?.unwrap_or_default();
    let needs_aeb_kick = endpoint_id.is_empty() || !all_served(&endpoint_id, pad_index);
    Ok(Some(PadEndpoint {
        endpoint_id,
        device_instance,
        pad_index,
        needs_aeb_kick,
    }))
}

/// `pad-endpoint status` devtest body: devnode, endpoint, and per-stamp stored/served state.
pub(crate) fn print_status(pad_index: u8) -> Result<()> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")?;
    let Some(inst) = find_devnode(pad_index)? else {
        println!("pad {pad_index}: no pad-audio devnode");
        return Ok(());
    };
    println!("pad {pad_index}: devnode {inst}");
    let Some(ep) = find_endpoint_for_devnode(&inst)? else {
        println!("  endpoint: NONE (driver not installed, or the endpoint never registered)");
        return Ok(());
    };
    println!("  endpoint: {ep}");
    let props = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(format!(
            r"{MMDEV_RENDER_PATH}\{}\Properties",
            endpoint_guid_part(&ep)?
        ))
        .context("open the endpoint's Properties key")?;
    let dev = open_mmdevice(&ep)?;
    // SAFETY: read-only property store on the MTA-initialized current thread.
    let store = unsafe { dev.OpenPropertyStore(STGM_READ) }.context("OpenPropertyStore")?;
    let mut all = true;
    for s in active_stamps(pad_index) {
        let stored = match &s.value {
            StampValue::Str(v) => props
                .get_value::<String, _>(reg_value_name(&s.key))
                .map(|got| got == *v)
                .unwrap_or(false),
            other => props
                .get_raw_value(reg_value_name(&s.key))
                .map(|rv| rv.bytes == reg_registry_value(other).bytes)
                .unwrap_or(false),
        };
        let served = stamp_served(&store, &s);
        all &= served;
        println!("  {:<14} stored={stored} served={served}", s.label);
    }
    println!("  needs_aeb_kick={}", !all);
    Ok(())
}

// --- wiring-plan exclusion data -------------------------------------------------------------

/// Is this render endpoint one of ours (a pad "speaker" that must never become a mic target,
/// loopback source, or default device)? Registry-only — callable off the COM threads and cheap
/// enough for every wiring pass. Rules (either suffices):
///
/// * the STORED ContainerId is a PFDS container (`Data1 == "PFDS"`), or
/// * the endpoint's devnode is one of ours — the `PunktfunkPadIndex` marker (or, pre-install,
///   the creation DeviceDesc).
///
/// Positives are cached (a pad endpoint never becomes a normal one); negatives are recomputed
/// each time, because an endpoint the wiring plan saw BEFORE `ensure()` stamped it must flip
/// to excluded on the next pass.
pub(crate) fn is_pad_render_endpoint(endpoint_id: &str) -> bool {
    static KNOWN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let known = KNOWN.get_or_init(|| Mutex::new(HashSet::new()));
    if known.lock().unwrap().contains(endpoint_id) {
        return true;
    }
    let is_pad = compute_is_pad_endpoint(endpoint_id);
    if is_pad {
        known.lock().unwrap().insert(endpoint_id.to_string());
    }
    is_pad
}

fn compute_is_pad_endpoint(endpoint_id: &str) -> bool {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    let Ok(guid) = endpoint_guid_part(endpoint_id) else {
        return false;
    };
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(props) = hklm.open_subkey(format!(r"{MMDEV_RENDER_PATH}\{guid}\Properties")) else {
        return false;
    };
    // (a) stamped container: serialized VT_CLSID whose Data1 spells "PFDS".
    if let Ok(v) = props.get_raw_value(reg_value_name(&PKEY_CONTAINER_ID)) {
        if v.bytes.len() >= 12 && v.bytes[0] == 0x48 && v.bytes[8..12] == PFDS_TAG.to_le_bytes() {
            return true;
        }
    }
    // (b) created-but-not-yet-stamped: the owning devnode carries our marker.
    let Ok(link) = props.get_value::<String, _>(reg_value_name(&PKEY_ENDPOINT_DEVNODE)) else {
        return false;
    };
    let Some(inst) = link.strip_prefix("{1}.") else {
        return false;
    };
    let enum_key = format!(r"SYSTEM\CurrentControlSet\Enum\{inst}");
    if hklm
        .open_subkey(format!(r"{enum_key}\Device Parameters"))
        .and_then(|k| k.get_raw_value(PAD_INDEX_VALUE))
        .is_ok()
    {
        return true;
    }
    hklm.open_subkey(&enum_key)
        .and_then(|k| k.get_value::<String, _>("DeviceDesc"))
        .map(|d| d == DEVNODE_DESC)
        .unwrap_or(false)
}

// --- host-startup integration ---------------------------------------------------------------

/// `PUNKTFUNK_PAD_AUDIO`: unset or anything but "0" = on; "0" = off.
fn pad_audio_enabled() -> bool {
    std::env::var_os("PUNKTFUNK_PAD_AUDIO").is_none_or(|v| v != "0")
}

/// `PUNKTFUNK_PAD_AUDIO_SLOTS`: how many pad slots to pre-provision (default 1, max 4).
fn pad_audio_slots() -> u8 {
    std::env::var("PUNKTFUNK_PAD_AUDIO_SLOTS")
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(1)
        .clamp(1, 4)
}

/// The endpoints provisioned at startup, set exactly once by the worker thread — and only on
/// SUCCESS. See [`provision_at_startup`] for why the failure path deliberately leaves it unset.
static PROVISIONED: OnceLock<Arc<Vec<PadEndpoint>>> = OnceLock::new();

/// A provisioning attempt is in flight. Guards the retry in [`ensure_provisioned`] against
/// spawning a second COM worker while the first is still enumerating.
static PROVISIONING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Host-startup pre-provisioning (Windows, env-gated): spawn a COM worker that `ensure()`s
/// endpoints for slots `0..N`, performs at most ONE AudioEndpointBuilder+Audiosrv restart if
/// any stamp is stored-but-not-served (then re-verifies), and publishes the results for the
/// session layer ([`endpoint_for`]). Any failure logs one warning and leaves the feature off —
/// the pad itself keeps working, just without audio.
pub(crate) fn provision_at_startup() {
    if !pad_audio_enabled() {
        tracing::info!("pad audio disabled (PUNKTFUNK_PAD_AUDIO=0)");
        return;
    }
    if PROVISIONED.get().is_some() {
        return;
    }
    // R5: one attempt at a time. Without this the retry below could stack COM workers.
    if PROVISIONING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let slots = pad_audio_slots();
    let spawned = thread::Builder::new()
        .name("punktfunk-pad-audio".into())
        .spawn(move || {
            let mut eps: Vec<PadEndpoint> = Vec::new();
            for idx in 0..slots {
                match ensure(idx) {
                    Ok(pe) => {
                        tracing::info!(pad = idx, endpoint = %pe.endpoint_id,
                            needs_aeb_kick = pe.needs_aeb_kick, "pad-audio endpoint ready");
                        eps.push(pe);
                    }
                    Err(e) => {
                        tracing::warn!(pad = idx, error = %format!("{e:#}"),
                            "pad-audio endpoint provisioning failed — pad audio unavailable \
                             (pads still work, without a pad speaker)");
                        break;
                    }
                }
            }
            if eps.iter().any(|p| p.needs_aeb_kick) {
                // One restart, at startup, before any session exists — never mid-flight.
                match restart_audio_endpoint_services() {
                    Ok(()) => {
                        for pe in &mut eps {
                            pe.needs_aeb_kick = !all_served(&pe.endpoint_id, pe.pad_index);
                            if pe.needs_aeb_kick {
                                tracing::warn!(pad = pe.pad_index, endpoint = %pe.endpoint_id,
                                    "pad endpoint stamps still not served after the audio-stack \
                                     restart");
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %format!("{e:#}"),
                        "could not restart the audio stack for the pad endpoints — stamps stay \
                         stored-but-not-served until the next reboot"),
                }
            }
            // R5: latch the result ONLY if we actually provisioned something. This used to store
            // whatever `eps` held even when the loop broke on the first error — an empty vec —
            // and `OnceLock` made that permanent: one transient failure (a busy audio stack, a
            // service mid-restart) disabled pad audio for the entire life of the host process,
            // with the only evidence a single warning at startup. An empty result now leaves the
            // cell unset so `ensure_provisioned` can try again when a session next asks.
            if eps.is_empty() {
                tracing::warn!(
                    "pad-audio provisioning produced no endpoints — leaving it unlatched so the \
                     next session retries rather than disabling pad audio for this process"
                );
            } else {
                let _ = PROVISIONED.set(Arc::new(eps));
            }
            PROVISIONING.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    if let Err(e) = spawned {
        PROVISIONING.store(false, std::sync::atomic::Ordering::SeqCst);
        tracing::warn!(error = %e, "could not spawn the pad-audio provisioning thread");
    }
}

/// All endpoints provisioned at startup (`None` while provisioning runs / when disabled).
#[allow(dead_code)]
pub(crate) fn provisioned_endpoints() -> Option<Arc<Vec<PadEndpoint>>> {
    PROVISIONED.get().cloned()
}

/// R5: ask for provisioning again if the startup attempt produced nothing. Cheap and idempotent —
/// a successful latch returns immediately, and `PROVISIONING` keeps concurrent askers to one
/// worker. Called where a session first wants to know whether pad audio exists, so a host that
/// started while the audio stack was busy recovers on the next connect instead of at the next
/// reboot.
pub(crate) fn ensure_provisioned() {
    if PROVISIONED.get().is_none() {
        provision_at_startup();
    }
}

/// The provisioned endpoint for one pad slot — what a session queries when a client pad with
/// speaker support arrives, to attach a [`PadLoopbackCapturer`].
#[allow(dead_code)]
pub(crate) fn endpoint_for(pad_index: u8) -> Option<PadEndpoint> {
    PROVISIONED
        .get()?
        .iter()
        .find(|p| p.pad_index == pad_index)
        .cloned()
}

/// Restart AudioEndpointBuilder + Audiosrv (dependency order: the dependent Audiosrv stops
/// first, starts last) so registry-routed stamps get served. Mirrors the SCM idioms of
/// `service.rs::restart`.
fn restart_audio_endpoint_services() -> Result<()> {
    use windows_service::service::{Service, ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    fn stop_and_wait(svc: &Service, name: &str) -> Result<()> {
        let _ = svc.stop(); // ERROR_SERVICE_NOT_ACTIVE just means it is already down
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let state = svc
                .query_status()
                .with_context(|| format!("query {name} status"))?;
            if state.current_state == ServiceState::Stopped {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("{name} did not stop within 20 s");
            }
            thread::sleep(Duration::from_millis(250));
        }
    }

    tracing::info!(
        "restarting AudioEndpointBuilder + Audiosrv once so the pad endpoints serve their \
         stamped identity (host startup, before any session)"
    );
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open Service Control Manager")?;
    let access = ServiceAccess::STOP | ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let audiosrv = mgr
        .open_service("Audiosrv", access)
        .context("open Audiosrv")?;
    let aeb = mgr
        .open_service("AudioEndpointBuilder", access)
        .context("open AudioEndpointBuilder")?;
    stop_and_wait(&audiosrv, "Audiosrv")?;
    stop_and_wait(&aeb, "AudioEndpointBuilder")?;
    aeb.start(&[] as &[&std::ffi::OsStr])
        .context("start AudioEndpointBuilder")?;
    audiosrv
        .start(&[] as &[&std::ffi::OsStr])
        .context("start Audiosrv")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let state = audiosrv.query_status().context("query Audiosrv status")?;
        if state.current_state == ServiceState::Running {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Audiosrv did not come back within 20 s");
        }
        thread::sleep(Duration::from_millis(250));
    }
}

// --- loopback capture -----------------------------------------------------------------------

/// The pad endpoint's channel count (quad).
pub const PAD_CHANNELS: u32 = 4;
/// `dwChannelMask` for the 4-ch pad layout (FL FR BL BR) — what the endpoint is stamped with.
/// NOT `punktfunk_core::audio::wasapi_channel_mask`, which only speaks the GameStream
/// stereo/5.1/7.1 layouts.
const PAD_CHANNEL_MASK: u32 = 0x33;
/// 4 ch × f32.
const PAD_BLOCK_ALIGN: usize = PAD_CHANNELS as usize * 4;

/// WASAPI loopback capture of one pad endpoint: whatever a game renders into the pad
/// "speaker" comes out as interleaved 4-ch f32 at 48 kHz, ready for the pad-audio downlink.
/// Same COM discipline as [`super::wasapi_cap`]: the WASAPI objects live on a dedicated
/// thread; the struct holds only the channel + stop flag + join handle. Any device error
/// (endpoint invalidated, engine restart) ends the thread — [`AudioCapturer::next_chunk`]
/// then returns `Err` and the caller reopens.
pub struct PadLoopbackCapturer {
    chunks: Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl PadLoopbackCapturer {
    /// Open a loopback capture on a provisioned pad endpoint (its full WASAPI endpoint id —
    /// [`PadEndpoint::endpoint_id`]).
    pub fn open(endpoint_id: &str) -> Result<PadLoopbackCapturer> {
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let stop = Arc::new(AtomicBool::new(false));
        // Bring-up handshake: surface an open failure as Err (caller retries/reopens), never a
        // silent dead thread — the crate-wide capture idiom.
        let (ready_tx, ready_rx) = sync_channel::<Result<()>>(1);
        let (stop_t, id) = (stop.clone(), endpoint_id.to_string());
        let join = thread::Builder::new()
            .name("punktfunk-pad-cap".into())
            .spawn(move || {
                if let Err(e) = pad_capture_thread(&id, tx, stop_t, ready_tx) {
                    tracing::error!(error = %format!("{e:#}"), "pad loopback thread failed");
                }
            })
            .context("spawn pad loopback thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(PadLoopbackCapturer {
                chunks: rx,
                stop,
                join: Some(join),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // R6: signal AND reap. Dropping `join` here detached the WASAPI thread, and the
                // streamer's reopen loop retries this every ~2 s — so a wedged activation leaked
                // one thread (each holding COM apartment state and a channel end) per attempt,
                // indefinitely. The join is bounded in practice because the thread's own loop
                // observes `stop` between waits; give it a moment and, if it is genuinely stuck
                // inside a blocking WASAPI call, say so rather than leaking in silence.
                stop.store(true, Ordering::SeqCst);
                match reap_with_timeout(join, Duration::from_secs(2)) {
                    true => Err(anyhow!("pad loopback init timed out")),
                    false => Err(anyhow!(
                        "pad loopback init timed out and its thread did not exit — the audio \
                         stack is wedged; not retrying into a thread leak"
                    )),
                }
            }
        }
    }
}

/// Join `join`, giving it `budget` to notice a stop flag. `true` if it exited.
///
/// A detached thread is the wrong answer at a retry point (see [`PadLoopbackCapturer::open`]):
/// the caller reopens on a timer, so "leak one and move on" compounds. Waiting is bounded, and a
/// thread that outlasts the budget is reported instead of silently accumulating.
fn reap_with_timeout(join: JoinHandle<()>, budget: Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while !join.is_finished() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = join.join();
    true
}

/// Render a test tone straight into a pad's audio endpoint.
///
/// The point is iteration speed. Without this, exercising the pad-audio chain means launching a
/// game that renders DualSense haptics and hoping it targets the right endpoint — minutes per
/// attempt, and a failure tells you nothing about *which* link broke. This drives the endpoint
/// directly, so the rest of the chain (loopback capture → gate → Opus → 0xD1 → client → the pad's
/// actuators) can be tested in seconds and in isolation from whether any game cooperates.
///
/// Which channel pair carries the tone. The pad's 4 channels split FL|FR|BL|BR: the FRONT pair is
/// the pad's built-in speaker, the BACK pair the voice coils. Driving one and leaving the other
/// silent is what makes a result attributable — the capture (and the client) can then say which
/// kind the framer actually routed, instead of "some audio arrived".
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum TonePair {
    /// The pad's built-in speaker.
    Front,
    /// The voice coils.
    Back,
    Both,
}

impl TonePair {
    /// Parse the `--pair` devtest argument; anything unrecognised keeps the haptics default.
    pub(crate) fn parse(s: &str) -> TonePair {
        match s {
            "front" | "speaker" => TonePair::Front,
            "both" => TonePair::Both,
            _ => TonePair::Back,
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            TonePair::Front => "FRONT pair (the pad's speaker)",
            TonePair::Back => "BACK pair (the voice coils)",
            TonePair::Both => "BOTH pairs (speaker + voice coils)",
        }
    }
    /// Does channel `c` (0..4, FL|FR|BL|BR) carry the tone?
    fn carries(self, c: usize) -> bool {
        match self {
            TonePair::Front => c < 2,
            TonePair::Back => c >= 2,
            TonePair::Both => true,
        }
    }
}

/// The tone defaults to the BACK channel pair, because that is the pair the framer routes to the
/// haptics kind — the voice coils. The front pair then stays silent, so a pass is felt in the grips
/// and cannot be confused with the pad's speaker. `--pair front` drives the speaker instead, which
/// is the only way to exercise the speaker kind without a game that renders one.
pub(crate) fn render_test_tone(
    endpoint_id: &str,
    seconds: u32,
    hz: f32,
    pair: TonePair,
) -> Result<()> {
    wasapi::initialize_mta()
        .ok()
        .context("initialize COM (MTA) for the tone render")?;

    probe_activation(endpoint_id);
    // By id, never a default-device resolve: the whole question being answered is whether THIS
    // endpoint is the one the capture sees.
    let device = open_wasapi_device(endpoint_id)
        .with_context(|| format!("pad endpoint {endpoint_id} not found"))?;
    let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
    // B11: the SAME mask the endpoint and the loopback capture use. Passing `None` let wasapi
    // derive `(1 << 4) - 1` = 0x0F (FL FR FC LFE) instead of 0x33 (FL FR BL BR), so this devtest
    // — the instrument for "which coil is which" — put its tone on a different channel pairing
    // than the real path. It could not exercise the coil route at all, and read as an inverted
    // pair when it appeared to.
    let desired = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        SAMPLE_RATE as usize,
        PAD_CHANNELS as usize,
        Some(PAD_CHANNEL_MASK),
    );
    let (default_period, _min) = audio_client.get_device_period().context("device period")?;
    audio_client
        .initialize_client(
            &desired,
            &Direction::Render,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: default_period,
            },
        )
        .context("initialize render client")?;
    let h_event = audio_client.set_get_eventhandle().context("event handle")?;
    let render = audio_client
        .get_audiorenderclient()
        .context("IAudioRenderClient")?;
    let buf_frames = audio_client.get_buffer_size().context("buffer size")? as usize;
    let block = PAD_CHANNELS as usize * std::mem::size_of::<f32>();
    // Start on silence so the stream opens without a glitch, exactly as the mic pump does.
    let _ = render.write_to_device(buf_frames, &vec![0u8; buf_frames * block], None);
    audio_client.start_stream().context("start render stream")?;

    let total = u64::from(SAMPLE_RATE) * u64::from(seconds.clamp(1, 60));
    let step = std::f32::consts::TAU * hz / SAMPLE_RATE as f32;
    let mut phase = 0.0f32;
    let mut written = 0u64;
    let mut bytes = vec![0u8; buf_frames * block];

    while written < total {
        if h_event.wait_for_event(1000).is_err() {
            anyhow::bail!("render event timed out after {written} frames");
        }
        let free = audio_client
            .get_available_space_in_frames()
            .context("available space")? as usize;
        let n = free.min((total - written) as usize);
        if n == 0 {
            continue;
        }
        for f in 0..n {
            let s = phase.sin() * 0.5;
            phase += step;
            if phase >= std::f32::consts::TAU {
                phase -= std::f32::consts::TAU;
            }
            for c in 0..PAD_CHANNELS as usize {
                let v: f32 = if pair.carries(c) { s } else { 0.0 };
                let at = (f * PAD_CHANNELS as usize + c) * 4;
                bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        render
            .write_to_device(n, &bytes[..n * block], None)
            .context("write tone")?;
        written += n as u64;
    }
    // Let the tail drain before tearing the stream down.
    std::thread::sleep(Duration::from_millis(200));
    let _ = audio_client.stop_stream();
    Ok(())
}

/// `pad-endpoint capture` devtest body: open the REAL loopback capture on a pad endpoint and
/// report what actually arrives, split by channel pair.
///
/// The other half of [`render_test_tone`]. Run the two together — tone in one process, this in
/// another — and the entire host side is exercised with no game and no client: a render client
/// opens the endpoint, the engine loops it back, and the capture must see the tone in the BACK
/// pair only. That is the exact signal the `0xD1` framer routes to the voice coils, so a pass here
/// means everything upstream of the wire is sound.
///
/// Reading the result: `peak_back` well above zero with `peak_front` at exactly zero is a pass.
/// Front energy means the pair routing is wrong. Silence in both means the endpoint provisioned
/// but carries no audio — which is what a stamped-but-unservable endpoint looked like, and is
/// precisely the state that used to be invisible until a client sat on an empty plane.
pub(crate) fn capture_probe(endpoint_id: &str, seconds: u32) -> Result<()> {
    let mut cap = PadLoopbackCapturer::open(endpoint_id)
        .with_context(|| format!("open pad loopback on {endpoint_id}"))?;
    let deadline = Instant::now() + Duration::from_secs(u64::from(seconds.clamp(1, 60)));
    let (mut frames, mut peak_front, mut peak_back) = (0u64, 0f32, 0f32);
    while Instant::now() < deadline {
        let chunk = cap.next_chunk().context("read pad loopback")?;
        for f in chunk.chunks_exact(PAD_CHANNELS as usize) {
            frames += 1;
            peak_front = peak_front.max(f[0].abs()).max(f[1].abs());
            peak_back = peak_back.max(f[2].abs()).max(f[3].abs());
        }
    }
    println!(
        "pad-endpoint capture: {frames} frames over {seconds}s, peak_front={peak_front:.4} \
         peak_back={peak_back:.4}"
    );
    // The capture cannot know which pair the tone was aimed at, so it reports what it SAW rather
    // than judging against an assumed one — an earlier haptics-shaped verdict called a perfectly
    // good `--pair front` run "silent".
    const FLOOR: f32 = 0.0001;
    match (frames, peak_front > FLOOR, peak_back > FLOOR) {
        (0, _, _) => println!("  VERDICT: FAIL — the capture opened but delivered nothing."),
        (_, false, false) => println!(
            "  VERDICT: silent — capture works, but nothing was rendered. Run `pad-endpoint \
             tone` against this endpoint at the same time."
        ),
        (_, false, true) => println!(
            "  VERDICT: BACK pair only (the voice coils), front silent — channel-exact for \
             haptics."
        ),
        (_, true, false) => println!(
            "  VERDICT: FRONT pair only (the pad's speaker), back silent — channel-exact for \
             the speaker."
        ),
        (_, true, true) => println!(
            "  VERDICT: BOTH pairs carry signal — correct for `--pair both`, otherwise the pairs \
             are leaking into each other."
        ),
    }
    Ok(())
}

impl Drop for PadLoopbackCapturer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl AudioCapturer for PadLoopbackCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        match self.chunks.recv_timeout(Duration::from_secs(5)) {
            Ok(c) => Ok(c),
            // A quiet pad (no game renders into it) is NOT a failure — empty chunk, keep the
            // capturer. Err is reserved for a dead capture thread (device invalidated), which
            // tells the caller to reopen.
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("pad loopback thread ended")),
        }
    }
    fn channels(&self) -> u32 {
        PAD_CHANNELS
    }
    fn drain(&mut self) {
        while self.chunks.try_recv().is_ok() {}
    }
}

fn pad_capture_thread(
    endpoint_id: &str,
    tx: SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    ready: SyncSender<Result<()>>,
) -> Result<()> {
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    // Open the endpoint EXPLICITLY by id (never a default-device resolve — pad endpoints must
    // never be anyone's default). 48 kHz 4-ch f32 with shared-mode autoconvert, so the engine
    // hands us the contract format regardless of the endpoint's current mix format; capturing
    // a RENDER device with Direction::Capture in shared mode is WASAPI loopback.
    let setup = (|| -> Result<(wasapi::AudioClient, wasapi::AudioCaptureClient, wasapi::Handle)> {
        let device = open_wasapi_device(endpoint_id)
            .map_err(|e| anyhow!("open pad endpoint {endpoint_id}: {e:#}"))?;
        let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
        let desired = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            SAMPLE_RATE as usize,
            PAD_CHANNELS as usize,
            Some(PAD_CHANNEL_MASK),
        );
        let (default_period, _min) = audio_client.get_device_period().context("device period")?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: default_period,
        };
        audio_client
            .initialize_client(&desired, &Direction::Capture, &mode)
            .context("initialize pad loopback client")?;
        let h_event = audio_client.set_get_eventhandle().context("event handle")?;
        let capture_client = audio_client
            .get_audiocaptureclient()
            .context("IAudioCaptureClient")?;
        audio_client.start_stream().context("start pad loopback")?;
        Ok((audio_client, capture_client, h_event))
    })();
    let (audio_client, capture_client, h_event) = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("{e:#}")));
            return Ok(());
        }
    };
    let _ = ready.send(Ok(()));
    tracing::info!(endpoint = %endpoint_id, "pad loopback capturing (4 ch / 48 kHz f32)");

    // Any error below (endpoint invalidated/removed, engine restart) ends the thread; the
    // channel disconnect surfaces as next_chunk() -> Err and the caller reopens.
    let mut bytes: VecDeque<u8> = VecDeque::new();
    while !stop.load(Ordering::Relaxed) {
        // Loopback fires events only while a game renders into the pad; the finite timeout
        // keeps `stop` responsive across silence.
        let _ = h_event.wait_for_event(100);
        loop {
            match capture_client.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(_n)) => {
                    capture_client
                        .read_from_device_to_deque(&mut bytes)
                        .context("read pad loopback")?;
                }
                Err(e) => return Err(anyhow!("get_next_packet_size: {e}")),
            }
        }
        let whole = (bytes.len() / PAD_BLOCK_ALIGN) * PAD_BLOCK_ALIGN;
        if whole > 0 {
            let raw: Vec<u8> = bytes.drain(..whole).collect();
            let mut samples = Vec::with_capacity(whole / 4);
            for c in raw.chunks_exact(4) {
                samples.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
            let _ = tx.try_send(samples); // non-blocking, lossy — the crate's capture discipline
        }
    }
    audio_client.stop_stream().ok();
    Ok(())
}

// --- pure-logic tests (no devnodes, no COM) -------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The serialized container blob for pad 0 must be byte-for-byte the on-glass-measured
    /// value, and byte 23 must be the pad index.
    #[test]
    fn container_registry_blob_matches_measured() {
        let v = reg_registry_value(&StampValue::Container(pfds_container_guid(0)));
        let expect: Vec<u8> = vec![
            0x48, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // VT_CLSID header
            0x53, 0x44, 0x46, 0x50, // "PFDS" little-endian Data1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(v.bytes.as_ref(), expect.as_slice());
        for idx in [1u8, 3, 7] {
            let v = reg_registry_value(&StampValue::Container(pfds_container_guid(idx)));
            assert_eq!(v.bytes.len(), 24);
            assert_eq!(v.bytes[23], idx, "byte 23 must be the pad index");
        }
    }

    /// Both WAVEFORMATEXTENSIBLE legs: 4 ch, 48 kHz, mask 0x33, and internally consistent
    /// block alignment / byte rate.
    #[test]
    fn wfx_blobs_are_4ch_48k() {
        for (wfx, bits) in [(&WFX_PCM16_4CH_48K, 16u16), (&WFX_F32_4CH_48K, 32u16)] {
            assert_eq!(wfx.len(), 40);
            let ch = u16::from_le_bytes([wfx[2], wfx[3]]);
            let rate = u32::from_le_bytes([wfx[4], wfx[5], wfx[6], wfx[7]]);
            let byte_rate = u32::from_le_bytes([wfx[8], wfx[9], wfx[10], wfx[11]]);
            let align = u16::from_le_bytes([wfx[12], wfx[13]]);
            let b = u16::from_le_bytes([wfx[14], wfx[15]]);
            let mask = u32::from_le_bytes([wfx[20], wfx[21], wfx[22], wfx[23]]);
            assert_eq!(ch, 4);
            assert_eq!(rate, 48_000);
            assert_eq!(b, bits);
            assert_eq!(align as u32, ch as u32 * bits as u32 / 8);
            assert_eq!(byte_rate, rate * align as u32);
            assert_eq!(mask, PAD_CHANNEL_MASK);
        }
        // The serialized registry shape adds the 8-byte VT_BLOB header.
        let v = reg_registry_value(&StampValue::Format(&WFX_F32_4CH_48K));
        assert_eq!(v.bytes.len(), 48);
        assert_eq!(&v.bytes[..8], &[0x41, 0, 0, 0, 1, 0, 0, 0]);
    }

    /// Registry value names use the lowercase `{fmtid},pid` spelling MMDevices uses.
    #[test]
    fn reg_value_names() {
        assert_eq!(
            reg_value_name(&PKEY_CONTAINER_ID),
            "{8c7ed206-3f8a-4827-b3ab-ae9e1faefc6c},2"
        );
        assert_eq!(
            reg_value_name(&PKEY_ENDPOINT_DEVNODE),
            "{b3f8fa53-0004-438e-9003-51a46e139bfc},2"
        );
        assert_eq!(
            reg_value_name(&PKEY_ENDPOINT_DEVICE_NAME),
            "{b3f8fa53-0004-438e-9003-51a46e139bfc},6"
        );
    }

    #[test]
    fn endpoint_guid_extraction() {
        let id = "{0.0.0.00000000}.{aeb07c72-0f2b-4d3c-9a08-2b4a01234567}";
        assert_eq!(
            endpoint_guid_part(id).unwrap(),
            "{aeb07c72-0f2b-4d3c-9a08-2b4a01234567}"
        );
        assert!(endpoint_guid_part("bogus").is_err());
    }

    /// The string stamps stay REG_SZ (UTF-16LE + NUL), not serialized blobs.
    #[test]
    fn string_stamp_is_reg_sz() {
        let v = reg_registry_value(&StampValue::Str("Wireless Controller"));
        assert_eq!(v.vtype, winreg::enums::REG_SZ);
        assert_eq!(v.bytes.len(), ("Wireless Controller".len() + 1) * 2);
        assert_eq!(&v.bytes[..2], &[b'W', 0]);
    }
}
