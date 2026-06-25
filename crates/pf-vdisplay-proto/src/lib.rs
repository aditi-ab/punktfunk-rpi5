//! Shared binary contract between the punktfunk host and the `pf-vdisplay` IddCx driver.
//!
//! Two planes:
//! * [`control`] — the low-frequency `DeviceIoControl` plane (add/remove a virtual monitor, pin the
//!   render adapter, keepalive, info, clear-all). Owned, clean, versioned — NOT the SudoVDA ABI.
//! * [`frame`] — the IDD-push frame transport: the host creates a ring of shared keyed-mutex textures
//!   (+ a header + a frame-ready event) and the driver opens them and publishes composited frames into
//!   them. This crate owns the [`frame::SharedHeader`] layout, the [`frame::FrameToken`] packing, the
//!   `Global\` object-name scheme, and the driver-status codes.
//!
//! Both planes were previously hand-duplicated, byte-for-byte, across `idd_push.rs`/`frame_transport.rs`
//! and `vdisplay/sudovda.rs`/`control.rs` with only "must match" comments guarding them. Defining them
//! once here — with bytemuck `Pod` derives and `const` size asserts — makes any drift a compile error.
//!
//! The GUID and LUID are carried as plain integers; the host converts to `windows::core::GUID` /
//! `windows::Win32::Foundation::LUID` and the driver to its own bindgen types via the same constants.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

/// Freshly-minted pf-vdisplay device-interface GUID — `{70667664-7044-5350-a1b2-c3d4e5f60001}`.
/// Deliberately NOT SudoVDA's `{e5bcc234-…}`: we own the driver, so a private interface GUID signals
/// it and removes any accidental coexistence with a real SudoVDA install. Construct on each side via
/// `GUID::from_u128(PF_VDISPLAY_INTERFACE_GUID_U128)`.
pub const PF_VDISPLAY_INTERFACE_GUID_U128: u128 = 0x7066_7664_7044_5350_a1b2_c3d4_e5f6_0001;

/// The interface GUID split into Windows `GUID` fields — `(Data1, Data2, Data3, Data4)` — so the driver
/// (and host) can build a `windows`/`wdk_sys` `GUID` without re-deriving the byte layout. Standard GUID
/// layout from the u128: `Data1` = high 32 bits, `Data2`/`Data3` = next two 16-bit groups, `Data4` =
/// the low 64 bits big-endian. (This crate is `no_std` + provider-agnostic, so it returns the fields
/// rather than depend on a `GUID` type.)
#[must_use]
pub const fn interface_guid_fields() -> (u32, u16, u16, [u8; 8]) {
    let g = PF_VDISPLAY_INTERFACE_GUID_U128;
    (
        (g >> 96) as u32,
        (g >> 80) as u16,
        (g >> 64) as u16,
        (g as u64).to_be_bytes(),
    )
}

/// Bumped on any incompatible change to either plane. Exchanged via [`control::IOCTL_GET_INFO`]; host
/// and driver assert a match at startup so a mismatched pair fails loudly instead of corrupting.
pub const PROTOCOL_VERSION: u32 = 1;

/// `CTL_CODE(FILE_DEVICE_UNKNOWN = 0x22, func, METHOD_BUFFERED = 0, FILE_ANY_ACCESS = 0)`.
pub const fn ctl_code(func: u32) -> u32 {
    (0x22u32 << 16) | (func << 2)
}

/// The control (`DeviceIoControl`) plane: add/remove a virtual monitor + adapter pin + keepalive.
pub mod control {
    use super::ctl_code;
    use bytemuck::{Pod, Zeroable};

    // Contiguous op space at 0x900 — distinct from SudoVDA's gappy 0x800/0x888/0x8FF numbering.
    /// Add a virtual monitor at a mode → [`AddReply`]. Input [`AddRequest`].
    pub const IOCTL_ADD: u32 = ctl_code(0x900);
    /// Remove a virtual monitor by session id. Input [`RemoveRequest`].
    pub const IOCTL_REMOVE: u32 = ctl_code(0x901);
    /// Pin the IddCx render adapter (hybrid-GPU IDD-push). Input [`SetRenderAdapterRequest`].
    pub const IOCTL_SET_RENDER_ADAPTER: u32 = ctl_code(0x902);
    /// Keepalive (resets the driver watchdog). No payload.
    pub const IOCTL_PING: u32 = ctl_code(0x903);
    /// Version + watchdog handshake → [`InfoReply`]. No input.
    pub const IOCTL_GET_INFO: u32 = ctl_code(0x904);
    /// Tear down every virtual monitor (host-startup orphan reap). No payload. First-class op — NOT the
    /// SudoVDA "send-and-hope-it's-ignored" hack.
    pub const IOCTL_CLEAR_ALL: u32 = ctl_code(0x905);

    /// `IOCTL_ADD` input. A monotonic `session_id` keys the monitor (the host's refcount manager owns
    /// collision safety — no more SudoVDA's 16-byte GUID + pid-mangling). The driver advertises this
    /// mode as preferred; the host still CCD-forces the active mode (the OS activates IDDs at a default).
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct AddRequest {
        pub session_id: u64,
        pub width: u32,
        pub height: u32,
        pub refresh_hz: u32,
        pub _reserved: u32,
    }

    /// `IOCTL_ADD` reply: the OS target id + the adapter LUID the IDD landed on (split low/high to
    /// match `windows` `LUID { LowPart: u32, HighPart: i32 }`).
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct AddReply {
        pub adapter_luid_low: u32,
        pub adapter_luid_high: i32,
        pub target_id: u32,
        pub _reserved: u32,
    }

    /// `IOCTL_REMOVE` input.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct RemoveRequest {
        pub session_id: u64,
    }

    /// `IOCTL_SET_RENDER_ADAPTER` input (the GPU the IddCx swap-chain should render on).
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct SetRenderAdapterRequest {
        pub luid_low: u32,
        pub luid_high: i32,
    }

    /// `IOCTL_GET_INFO` reply: the protocol version (asserted against [`super::PROTOCOL_VERSION`]) and
    /// the watchdog timeout the host must ping within.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct InfoReply {
        pub protocol_version: u32,
        pub watchdog_timeout_s: u32,
    }

    // Layout is load-bearing across the process boundary — pin it. (bytemuck's Pod derive already
    // rejects any internal padding; these assert the externally-visible sizes too.) The `offset_of!`
    // asserts additionally catch a SAME-SIZE field reorder, which the size+Pod checks alone miss.
    const _: () = {
        use core::mem::{offset_of, size_of};

        assert!(size_of::<AddRequest>() == 24);
        assert!(offset_of!(AddRequest, session_id) == 0);
        assert!(offset_of!(AddRequest, width) == 8);
        assert!(offset_of!(AddRequest, height) == 12);
        assert!(offset_of!(AddRequest, refresh_hz) == 16);

        assert!(size_of::<AddReply>() == 16);
        assert!(offset_of!(AddReply, adapter_luid_low) == 0);
        assert!(offset_of!(AddReply, adapter_luid_high) == 4);
        assert!(offset_of!(AddReply, target_id) == 8);

        assert!(size_of::<RemoveRequest>() == 8);
        assert!(offset_of!(RemoveRequest, session_id) == 0);

        assert!(size_of::<SetRenderAdapterRequest>() == 8);
        assert!(offset_of!(SetRenderAdapterRequest, luid_low) == 0);
        assert!(offset_of!(SetRenderAdapterRequest, luid_high) == 4);

        assert!(size_of::<InfoReply>() == 8);
        assert!(offset_of!(InfoReply, protocol_version) == 0);
        assert!(offset_of!(InfoReply, watchdog_timeout_s) == 4);
    };
}

/// The IDD-push frame transport: the host-created shared ring header, the publish token, the names, and
/// the driver-status codes. The texture ring itself is host-created D3D11 keyed-mutex textures (opened
/// by name on the driver side); only the *layout/contract* lives here.
pub mod frame {
    use alloc::string::String;
    use bytemuck::{Pod, Zeroable};

    /// Header magic (`"PFVD"` LE). The host stamps it LAST (after the ring textures exist) so the driver
    /// only attaches to a fully-published ring.
    pub const MAGIC: u32 = 0x4456_4650;
    /// Frame-plane version (independent bump of the header layout).
    pub const VERSION: u32 = 1;
    /// Ring slots. Headroom so the driver's 0 ms-timeout publish always finds a free slot while the host
    /// holds one across the convert/copy + the pipelined encode. MUST be identical on both sides — it is,
    /// because both read this one constant.
    pub const RING_LEN: u32 = 6;

    /// `driver_status` values the driver writes into the host header (the host logs them on a timeout).
    pub const DRV_STATUS_NONE: u32 = 0;
    /// Driver attached to the ring and is publishing.
    pub const DRV_STATUS_OPENED: u32 = 1;
    /// Driver could not open the host's textures — render-adapter mismatch (it renders on a different GPU
    /// than where the host created the ring). `driver_status_detail` carries the HRESULT.
    pub const DRV_STATUS_TEX_FAIL: u32 = 2;
    /// Driver has no `ID3D11Device1` to open shared resources.
    pub const DRV_STATUS_NO_DEVICE1: u32 = 3;

    /// The shared metadata header (host-created, mapped by both sides). Atomic fields (`magic`, `latest`,
    /// `generation`) are accessed via each side's own atomic view over the mapping; this is the layout.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug)]
    pub struct SharedHeader {
        pub magic: u32,
        pub version: u32,
        /// Bumped by the host on a ring recreate (HDR-mode flip → new texture format/names). The driver
        /// re-attaches when it changes; a publish carries it so the host rejects a stale-ring publish.
        pub generation: u32,
        pub ring_len: u32,
        pub width: u32,
        pub height: u32,
        pub dxgi_format: u32,
        pub _pad: u32,
        /// Driver-written after each copy; host loads `Acquire`. See [`FrameToken`].
        pub latest: u64,
        pub qpc_pts: u64,
        /// Driver-written: the adapter the swap-chain actually renders on (mismatch detection).
        pub driver_render_luid_low: u32,
        pub driver_render_luid_high: i32,
        /// Driver-written status (visibility channel — UMDF hides OutputDebugString + the restricted
        /// token blocks file writes, so this header is how the driver reports state).
        pub driver_status: u32,
        pub driver_status_detail: u32,
    }

    /// The `SharedHeader.latest` publish token: `(generation << 40) | (seq << 8) | slot`.
    /// `generation` is 24-bit, `seq` 32-bit, `slot` 8-bit. The generation tag lets the host REJECT a
    /// publish from a stale ring (an old-generation publisher racing a mid-session recreate) so it never
    /// consumes an unwritten new-ring slot — eliminating the toggle-time garbage frame.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct FrameToken {
        pub generation: u32,
        pub seq: u32,
        pub slot: u8,
    }

    impl FrameToken {
        /// Low 24 bits of `generation` are significant (see the field docs).
        pub const GENERATION_MASK: u32 = 0x00FF_FFFF;

        pub const fn pack(self) -> u64 {
            (((self.generation & Self::GENERATION_MASK) as u64) << 40)
                | (((self.seq as u64) & 0xFFFF_FFFF) << 8)
                | (self.slot as u64)
        }

        pub const fn unpack(v: u64) -> Self {
            Self {
                generation: ((v >> 40) as u32) & Self::GENERATION_MASK,
                seq: ((v >> 8) & 0xFFFF_FFFF) as u32,
                slot: (v & 0xFF) as u8,
            }
        }
    }

    /// `Global\pfvd-hdr-<target>` — the shared metadata header mapping name.
    pub fn header_name(target_id: u32) -> String {
        alloc::format!("Global\\pfvd-hdr-{target_id}")
    }
    /// `Global\pfvd-evt-<target>` — the frame-ready auto-reset event name.
    pub fn event_name(target_id: u32) -> String {
        alloc::format!("Global\\pfvd-evt-{target_id}")
    }
    /// `Global\pfvd-tex-<target>-<generation>-<slot>` — a ring texture's shared-handle name. The
    /// generation in the name means a recreate's new textures never collide with the old ring's
    /// not-yet-released handles.
    pub fn texture_name(target_id: u32, generation: u32, slot: u32) -> String {
        alloc::format!("Global\\pfvd-tex-{target_id}-{generation}-{slot}")
    }

    // Size + per-field offsets are load-bearing: both sides access these via raw atomic views over the
    // mapping, so a same-size field reorder would silently corrupt. Pin every offset. The `_pad` after
    // `dxgi_format` is what 8-aligns the `u64 latest` at offset 32 — assert that too.
    const _: () = {
        use core::mem::{offset_of, size_of};

        assert!(size_of::<SharedHeader>() == 64);
        assert!(offset_of!(SharedHeader, magic) == 0);
        assert!(offset_of!(SharedHeader, version) == 4);
        assert!(offset_of!(SharedHeader, generation) == 8);
        assert!(offset_of!(SharedHeader, ring_len) == 12);
        assert!(offset_of!(SharedHeader, width) == 16);
        assert!(offset_of!(SharedHeader, height) == 20);
        assert!(offset_of!(SharedHeader, dxgi_format) == 24);
        assert!(offset_of!(SharedHeader, _pad) == 28);
        assert!(offset_of!(SharedHeader, latest) == 32);
        assert!(offset_of!(SharedHeader, qpc_pts) == 40);
        assert!(offset_of!(SharedHeader, driver_render_luid_low) == 48);
        assert!(offset_of!(SharedHeader, driver_render_luid_high) == 52);
        assert!(offset_of!(SharedHeader, driver_status) == 56);
        assert!(offset_of!(SharedHeader, driver_status_detail) == 60);
    };
}

/// Gamepad shared-memory layouts (host ↔ the UMDF gamepad drivers `pf_xusb` / `pf_dualsense`).
///
/// These were hand-duplicated as `OFF_*`/`SHM_*` constants in `inject/{gamepad,dualsense}_windows.rs`
/// and (as bare literals — `*view.add(140)`) in the standalone `xusb-driver`/`dualsense-driver`
/// workspaces, guarded only by "must match" comments — the top ABI-drift hazard the audit flagged
/// (`docs/windows-host-rewrite-audit.md` §6.1). Owning them here with `Pod` derives + `offset_of!`
/// asserts makes a one-sided edit a compile error.
///
/// The host creates the section (privileged, permissive DACL so the restricted WUDFHost token can
/// open it) and the driver maps it. Layout only; the section itself is host-created shared memory.
pub mod gamepad {
    use alloc::string::String;
    use bytemuck::{Pod, Zeroable};

    /// XUSB section magic — the exact u32 the shipped host + `pf_xusb` driver compare (loosely "PFXU").
    pub const XUSB_MAGIC: u32 = 0x5558_4650;
    /// Pad section magic — the exact u32 the shipped host + `pf_dualsense` driver compare (loosely
    /// "PFDS"). (Note: the two magics happen to use opposite byte-order mnemonics in the legacy code;
    /// only the u32 value is the contract.)
    pub const PAD_MAGIC: u32 = 0x5046_4453;

    /// `device_type` selector the `pf_dualsense` driver reads to pick its HID identity. The section is
    /// zeroed, so `0` = DualSense is the default; one driver serves either identity.
    pub const DEVTYPE_DUALSENSE: u8 = 0;
    /// `device_type` = DualShock 4 (`VID_054C&PID_09CC` HID identity).
    pub const DEVTYPE_DUALSHOCK4: u8 = 1;

    /// `Global\pfxusb-shm-<index>` — the virtual Xbox 360 (XInput) shared section.
    pub fn xusb_shm_name(index: u8) -> String {
        alloc::format!("Global\\pfxusb-shm-{index}")
    }
    /// `Global\pfds-shm-<index>` — the virtual DualSense / DualShock 4 shared section.
    pub fn pad_shm_name(index: u8) -> String {
        alloc::format!("Global\\pfds-shm-{index}")
    }

    /// Virtual Xbox 360 (XInput) shared section (64 B). The host writes the XInput state (a bumped
    /// `packet` number + buttons/triggers/sticks in XInput conventions); the driver answers
    /// `XInputGetState`. The driver writes force-feedback (`XInputSetState`) into `rumble_*`, bumping
    /// `rumble_seq`, which the host relays to the client.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug)]
    pub struct XusbShm {
        pub magic: u32,
        /// XInput `dwPacketNumber` — bumped by the host on every state change.
        pub packet: u32,
        pub buttons: u16,
        pub left_trigger: u8,
        pub right_trigger: u8,
        pub thumb_lx: i16,
        pub thumb_ly: i16,
        pub thumb_rx: i16,
        pub thumb_ry: i16,
        pub _reserved0: u32,
        /// Bumped by the driver on a new force-feedback packet.
        pub rumble_seq: u32,
        pub rumble_large: u8,
        pub rumble_small: u8,
        pub _reserved1: [u8; 34],
    }

    /// Virtual DualSense / DualShock 4 shared section (256 B). The host writes the `0x01`-style HID
    /// input report into `input`; the driver feeds it to game `READ_REPORT`s and publishes a game's
    /// `0x02` output (rumble / lightbar / player-LEDs / adaptive triggers) into `output`, bumping
    /// `out_seq`. `device_type` selects the HID identity ([`DEVTYPE_DUALSENSE`] / [`DEVTYPE_DUALSHOCK4`]).
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug)]
    pub struct PadShm {
        pub magic: u32,
        pub _reserved0: u32,
        /// Input report region (host-written; the codec's report is <= 64 B — see
        /// `inject::dualsense_proto::DS_INPUT_REPORT_LEN`). The region spans `magic`+pad .. `out_seq`.
        pub input: [u8; 64],
        /// Bumped by the driver when it publishes a new `output` report.
        pub out_seq: u32,
        /// Output report region (driver-written): rumble / lightbar / player-LEDs / adaptive triggers.
        pub output: [u8; 64],
        /// HID identity selector — see [`DEVTYPE_DUALSENSE`] / [`DEVTYPE_DUALSHOCK4`].
        pub device_type: u8,
        pub _reserved1: [u8; 115],
    }

    // Offsets are the wire contract the shipped drivers already read by hand — pin every one. A failing
    // assert here means the struct no longer matches the historical `OFF_*` layout (host) / `view.add(N)`
    // literal (driver) and must be fixed before either side switches to the type.
    const _: () = {
        use core::mem::{offset_of, size_of};

        assert!(size_of::<XusbShm>() == 64);
        assert!(offset_of!(XusbShm, magic) == 0);
        assert!(offset_of!(XusbShm, packet) == 4);
        assert!(offset_of!(XusbShm, buttons) == 8);
        assert!(offset_of!(XusbShm, left_trigger) == 10);
        assert!(offset_of!(XusbShm, right_trigger) == 11);
        assert!(offset_of!(XusbShm, thumb_lx) == 12);
        assert!(offset_of!(XusbShm, thumb_ly) == 14);
        assert!(offset_of!(XusbShm, thumb_rx) == 16);
        assert!(offset_of!(XusbShm, thumb_ry) == 18);
        assert!(offset_of!(XusbShm, rumble_seq) == 24);
        assert!(offset_of!(XusbShm, rumble_large) == 28);
        assert!(offset_of!(XusbShm, rumble_small) == 29);

        assert!(size_of::<PadShm>() == 256);
        assert!(offset_of!(PadShm, magic) == 0);
        assert!(offset_of!(PadShm, input) == 8);
        assert!(offset_of!(PadShm, out_seq) == 72);
        assert!(offset_of!(PadShm, output) == 76);
        assert!(offset_of!(PadShm, device_type) == 140);
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    #[test]
    fn frame_token_roundtrips() {
        for (g, s, slot) in [
            (1u32, 0u32, 0u8),
            (5, 12_345, 3),
            (frame::FrameToken::GENERATION_MASK, 0xFFFF_FFFF, 5),
            (0, 1, 255),
        ] {
            let t = frame::FrameToken {
                generation: g,
                seq: s,
                slot,
            };
            assert_eq!(frame::FrameToken::unpack(t.pack()), t);
        }
    }

    #[test]
    fn frame_token_packing_matches_legacy_layout() {
        // The legacy code packed (gen<<40)|(seq<<8)|slot by hand; lock the bit positions.
        let t = frame::FrameToken {
            generation: 7,
            seq: 42,
            slot: 3,
        };
        assert_eq!(t.pack(), (7u64 << 40) | (42u64 << 8) | 3u64);
    }

    #[test]
    fn shared_header_is_pod_and_64_bytes() {
        let mut h = frame::SharedHeader::zeroed();
        h.magic = frame::MAGIC;
        h.width = 5120;
        h.height = 1440;
        let bytes = bytemuck::bytes_of(&h);
        assert_eq!(bytes.len(), 64);
        let back: frame::SharedHeader = *bytemuck::from_bytes(bytes);
        assert_eq!(back.magic, frame::MAGIC);
        assert_eq!(back.width, 5120);
        assert_eq!(back.height, 1440);
    }

    #[test]
    fn control_structs_roundtrip_through_bytes() {
        let req = control::AddRequest {
            session_id: 0xDEAD_BEEF_CAFE_F00D,
            width: 3840,
            height: 2160,
            refresh_hz: 120,
            _reserved: 0,
        };
        let bytes = bytemuck::bytes_of(&req);
        assert_eq!(bytes.len(), 24);
        assert_eq!(*bytemuck::from_bytes::<control::AddRequest>(bytes), req);
    }

    #[test]
    fn names_are_stable() {
        assert_eq!(frame::header_name(10), "Global\\pfvd-hdr-10");
        assert_eq!(frame::event_name(10), "Global\\pfvd-evt-10");
        assert_eq!(frame::texture_name(10, 3, 5), "Global\\pfvd-tex-10-3-5");
    }

    #[test]
    fn gamepad_names_and_magics_are_stable() {
        assert_eq!(gamepad::xusb_shm_name(0), "Global\\pfxusb-shm-0");
        assert_eq!(gamepad::pad_shm_name(2), "Global\\pfds-shm-2");
        // Lock the exact u32 magics the shipped host/drivers use (inject/{gamepad,dualsense}_windows.rs).
        assert_eq!(gamepad::XUSB_MAGIC, 0x5558_4650);
        assert_eq!(gamepad::PAD_MAGIC, 0x5046_4453);
    }

    #[test]
    fn ctl_codes_are_contiguous_and_distinct() {
        assert_eq!(control::IOCTL_ADD, ctl_code(0x900));
        let all = [
            control::IOCTL_ADD,
            control::IOCTL_REMOVE,
            control::IOCTL_SET_RENDER_ADAPTER,
            control::IOCTL_PING,
            control::IOCTL_GET_INFO,
            control::IOCTL_CLEAR_ALL,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn guid_is_not_sudovda() {
        const SUDOVDA: u128 = 0xE5BC_C234_1E0C_418A_A0D4_EF8B_7501_414D;
        assert_ne!(PF_VDISPLAY_INTERFACE_GUID_U128, SUDOVDA);
    }
}
