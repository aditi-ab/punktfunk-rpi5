//! Shared binary contract between the punktfunk host and the `pf-vdisplay` IddCx driver.
//!
//! Two planes:
//! * [`control`] — the low-frequency `DeviceIoControl` plane (add/remove a virtual monitor, pin the
//!   render adapter, keepalive, info, clear-all, deliver the frame channel). Owned, clean, versioned —
//!   NOT the SudoVDA ABI.
//! * [`frame`] — the IDD-push frame transport: the host creates a ring of **unnamed** shared
//!   keyed-mutex textures (+ a header + a frame-ready event), duplicates their handles into the
//!   driver's WUDFHost process and delivers the handle VALUES over
//!   [`control::IOCTL_SET_FRAME_CHANNEL`]; the driver publishes composited frames into them. There is
//!   deliberately no object-name scheme: an unnamed object cannot be enumerated, opened by name, or
//!   pre-created ("squatted") — only the two endpoint processes ever hold a handle to any frame object
//!   (the sealed channel, `design/idd-push-security.md`). This crate owns the [`frame::SharedHeader`]
//!   layout, the [`frame::FrameToken`] packing, the channel-delivery struct, and the driver-status
//!   codes.
//!
//! Both planes were previously hand-duplicated, byte-for-byte, across `idd_push.rs`/`frame_transport.rs`
//! and `vdisplay/sudovda.rs`/`control.rs` with only "must match" comments guarding them. Defining them
//! once here — with bytemuck `Pod` derives and `const` size asserts — makes any drift a compile error.
//!
//! The GUID and LUID are carried as plain integers; the host converts to `windows::core::GUID` /
//! `windows::Win32::Foundation::LUID` and the driver to its own bindgen types via the same constants.
#![forbid(unsafe_code)]
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
/// v2: the sealed frame channel — the frame objects are unnamed and delivered by handle duplication
/// ([`control::IOCTL_SET_FRAME_CHANNEL`]), and [`control::AddReply`] grew `wudf_pid` (the duplication
/// target). A v1 driver has no channel-delivery IOCTL and expects named objects, so the pairing is
/// incompatible by design.
/// v3: ring↔monitor binding hardening for parallel displays
/// (`design/windows-parallel-virtual-displays.md` §3): [`frame::SharedHeader`] names its monitor
/// (`target_id`, the former `_pad` — same size, same offsets) and the driver's publisher refuses to
/// attach a ring naming a different monitor ([`frame::DRV_STATUS_BIND_FAIL`], the gamepad channel's
/// `pad_index` validation applied to frames). A v2 host never stamps the field, so a v3 driver
/// against a v2 host would refuse every attach — lockstep by the handshake, as ever.
/// v4: ADDITIVE — [`control::IOCTL_UPDATE_MODES`] (the in-place mid-stream resize,
/// `design/first-frame-and-resize-latency.md` P2): the driver refreshes a LIVE monitor's advertised
/// target-mode list (`IddCxMonitorUpdateModes2`) so the OS can mode-set to an arbitrary new mode
/// without a REMOVE→ADD monitor hotplug. Nothing existing changed, so the host accepts a v3 driver
/// too ([`MIN_DRIVER_PROTOCOL_VERSION`]) and simply falls back to the re-arrival resize against it;
/// a v4 driver serving an older (v3-asserting) host fails that host's strict handshake — ship
/// driver+host together, as ever.
/// v5: ADDITIVE — the IddCx HARDWARE CURSOR channel (remote-desktop-sweep M2c):
/// [`control::AddRequest::hw_cursor`] (the former tail `_reserved` — same size, same offsets)
/// asks the driver to declare a hardware cursor for this monitor (DWM then EXCLUDES the pointer
/// from the desktop frame and delivers shape/position out-of-band), and
/// [`control::IOCTL_SET_CURSOR_CHANNEL`] delivers the host-created [`cursor::CursorShm`] section
/// the driver's cursor thread seqlock-publishes into. Nothing existing changed; the host gates
/// the feature on the handshake-reported version (`>= 5`) and keeps composited-cursor behavior
/// against older drivers.
/// v6: ADDITIVE — [`control::IOCTL_SET_CURSOR_FORWARD`] (the mid-stream cursor-render flip,
/// remote-desktop-sweep §8): the client's mouse-model flip (un)declares the hardware cursor on a
/// LIVE monitor, so the capture mouse model gets DWM's composited pointer back (full fidelity)
/// and the desktop model gets exclusion + forwarding. Nothing existing changed; against a v5
/// driver the unknown IOCTL fails and the host logs + keeps the declared-at-ADD behavior.
/// v6 tail ext (no bump, the `AddRequest` luminance-tail discipline):
/// [`control::AddReply::cursor_excluded`] — the driver reports whether its ADAPTER already
/// carries a hardware-cursor declare from an earlier session. A declare is IRREVOCABLE
/// (remote-desktop-sweep §8.6, proven on-glass) and its exclusion reaches EVERY later monitor of
/// the adapter, not just the declaring target (on-glass 2026-07-23: a declare on one target left
/// a different client's fresh target cursor-less): DWM never composites the software cursor back
/// into any of the adapter's frames until the adapter resets. The host uses the flag to
/// self-composite the pointer (GDI poller + blend) in sessions that never negotiate the
/// cursor channel — without it those sessions are silently cursor-less. Both skews degrade
/// cleanly: an old driver writes only the 20-byte reply prefix (host reads `0` = unknown/clean),
/// an old host retrieves a 20-byte buffer (driver writes just the prefix).
pub const PROTOCOL_VERSION: u32 = 6;

/// The OLDEST driver protocol this host still drives (v4 is additive over v3 — see the v4 note on
/// [`PROTOCOL_VERSION`]): a v3 driver lacks only `IOCTL_UPDATE_MODES`, which the host gates on the
/// handshake-reported version and covers with the re-arrival fallback.
pub const MIN_DRIVER_PROTOCOL_VERSION: u32 = 3;

/// `CTL_CODE(FILE_DEVICE_UNKNOWN = 0x22, func, METHOD_BUFFERED = 0, FILE_ANY_ACCESS = 0)`.
pub const fn ctl_code(func: u32) -> u32 {
    (0x22u32 << 16) | (func << 2)
}

/// The control (`DeviceIoControl`) plane: add/remove a virtual monitor + adapter pin + keepalive +
/// frame-channel delivery.
pub mod control {
    use super::ctl_code;
    use super::frame::RING_LEN;
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
    /// Deliver a monitor's IDD-push frame channel: the handle VALUES of the unnamed shared objects the
    /// host duplicated into the driver's WUDFHost process. Input [`SetFrameChannelRequest`]. Sent once
    /// after the ring is created and again on every mid-session ring recreate (HDR-mode flip).
    pub const IOCTL_SET_FRAME_CHANNEL: u32 = ctl_code(0x906);
    /// Refresh a LIVE monitor's advertised target-mode list to a new preferred mode (+ the built-in
    /// fallbacks) via `IddCxMonitorUpdateModes2` — the in-place mid-stream resize (v4,
    /// `design/first-frame-and-resize-latency.md` P2). Input [`UpdateModesRequest`]. The host then
    /// CCD-forces the new mode active on the SAME monitor: no REMOVE→ADD hotplug, the monitor's OS
    /// identity (saved per-monitor DPI) and the driver's swap-chain/stash machinery survive. A v3
    /// driver fails this unknown IOCTL → the host falls back to the re-arrival resize.
    pub const IOCTL_UPDATE_MODES: u32 = ctl_code(0x907);
    /// Deliver a monitor's hardware-cursor channel (v5): the handle VALUE of the unnamed
    /// [`cursor::CursorShm`](crate::cursor) file mapping the host duplicated into the WUDFHost
    /// (same delivery model as [`IOCTL_SET_FRAME_CHANNEL`], no event — the host polls the
    /// seqlock at its encode-tick pace). Sent once after ADD, only for a monitor whose
    /// [`AddRequest::hw_cursor`] was set. The driver maps it, calls
    /// `IddCxMonitorSetupHardwareCursor`, and starts its cursor-query thread. Input
    /// [`SetCursorChannelRequest`].
    pub const IOCTL_SET_CURSOR_CHANNEL: u32 = ctl_code(0x908);
    /// Flip a LIVE monitor's hardware-cursor declaration (v6, the mid-stream cursor-render
    /// flip): `enable = 1` re-declares (`IddCxMonitorSetupHardwareCursor` — DWM excludes the
    /// pointer, the query/shape machinery resumes), `enable = 0` un-declares (the driver stops
    /// re-declaring on mode commits and asks the OS to revert to the software cursor — DWM
    /// composites the pointer into the frame, the pre-channel behavior the capture mouse
    /// model wants). Only meaningful for a monitor whose cursor channel was delivered. Input
    /// [`SetCursorForwardRequest`].
    pub const IOCTL_SET_CURSOR_FORWARD: u32 = ctl_code(0x909);

    /// `IOCTL_ADD` input. A monotonic `session_id` keys the monitor (the host's refcount manager owns
    /// collision safety — no more SudoVDA's 16-byte GUID + pid-mangling). The driver advertises this
    /// mode as preferred; the host still CCD-forces the active mode (the OS activates IDDs at a default).
    ///
    /// **Size compatibility**: the client-HDR luminance tail (the three fields after
    /// `preferred_monitor_id`) was appended without a protocol bump because BOTH directions degrade
    /// cleanly: an un-upgraded driver reads the [`ADD_REQUEST_LEGACY_SIZE`]-byte prefix of a new
    /// host's request (its `read_input` accepts a larger buffer) and keeps its built-in EDID
    /// luminance; an upgraded driver accepts a legacy-size request and zero-fills the tail (`0` =
    /// unknown → the built-in defaults). Any FURTHER field must follow the same discipline.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct AddRequest {
        pub session_id: u64,
        pub width: u32,
        pub height: u32,
        pub refresh_hz: u32,
        /// Host-preferred per-client monitor id (`1..=15`) — the EDID serial / IddCx `ConnectorIndex` /
        /// `ContainerId` the driver names this monitor by. A given client (keyed by its cert fingerprint)
        /// gets a STABLE id across reconnects, so the OS device path + EDID stay identical and Windows
        /// reapplies that client's saved per-monitor config (DPI scaling). `0` = AUTO: the driver
        /// allocates the lowest-free id (the original slot-based behavior — used for anonymous/TOFU and
        /// GameStream sessions). Byte-compatible with the old `_reserved` (offset 20): an un-upgraded
        /// driver ignores it (→ auto), which the host detects via [`AddReply::resolved_monitor_id`].
        pub preferred_monitor_id: u32,
        /// The CLIENT display's peak luminance in nits — written into this monitor's EDID CTA-861.3
        /// HDR static-metadata block (Desired Content Max Luminance), so host apps and the OS
        /// tone-map to the panel the stream actually lands on instead of the driver's built-in
        /// ~1000-nit placeholder. `0` = unknown → the driver keeps its built-in default block.
        pub max_luminance_nits: u32,
        /// The client display's max frame-average luminance in nits (→ Desired Content Max
        /// Frame-average Luminance). `0` = unknown/not indicated.
        pub max_frame_avg_nits: u32,
        /// The client display's min luminance in MILLI-nits (0.001 cd/m² — the CTA min-luminance
        /// range lives well below 1 nit) → Desired Content Min Luminance. `0` = unknown.
        pub min_luminance_millinits: u32,
        /// Non-zero = declare an IddCx HARDWARE CURSOR for this monitor (v5, remote-desktop-sweep
        /// M2c): DWM stops compositing the pointer into the frame and the driver publishes
        /// shape/position into the [`cursor::CursorShm`](crate::cursor) section delivered by
        /// [`IOCTL_SET_CURSOR_CHANNEL`]. Byte-compatible with the old tail `_reserved` (offset 36):
        /// an un-upgraded driver ignores it (cursor stays composited — the host already gates on
        /// the handshake version, this is defense in depth), an un-upgraded host sends `0` (off).
        pub hw_cursor: u32,
    }

    /// [`AddRequest`]'s size before the client-HDR luminance tail — the prefix an un-upgraded
    /// driver reads and the whole request an un-upgraded host sends (see the struct docs).
    pub const ADD_REQUEST_LEGACY_SIZE: usize = 24;

    /// `IOCTL_ADD` reply: the OS target id + the adapter LUID the IDD landed on (split low/high to
    /// match `windows` `LUID { LowPart: u32, HighPart: i32 }`).
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct AddReply {
        pub adapter_luid_low: u32,
        pub adapter_luid_high: i32,
        pub target_id: u32,
        /// The monitor id the driver ACTUALLY used — echoes [`AddRequest::preferred_monitor_id`] when the
        /// preference was honored, or the auto-allocated id otherwise. Byte-compatible with the old
        /// `_reserved` (offset 12): an un-upgraded driver leaves it `0`, so the host can tell its
        /// preference was ignored (stale driver) and log it instead of silently losing per-client config.
        pub resolved_monitor_id: u32,
        /// The driver's own process id (the WUDFHost hosting `pf_vdisplay`) — the target the host
        /// duplicates the unnamed frame-object handles INTO (`OpenProcess(PROCESS_DUP_HANDLE)` +
        /// `DuplicateHandle`, then [`IOCTL_SET_FRAME_CHANNEL`]). Reported per-ADD, not per-open, so a
        /// WUDFHost restart between sessions can never leave the host duplicating into a dead process.
        pub wudf_pid: u32,
        /// Non-zero = the ADAPTER already carries an IRREVOCABLE hardware-cursor declare from an
        /// earlier session (remote-desktop-sweep §8.6; reach is adapter-wide, not per-target —
        /// on-glass 2026-07-23): DWM excludes the pointer from every frame on every monitor until
        /// the adapter resets, and a session without the cursor channel must
        /// self-composite (GDI poller + blend) or stream a cursor-less desktop. Appended after
        /// [`ADD_REPLY_LEGACY_SIZE`] under the same dual-size discipline as the `AddRequest`
        /// luminance tail: an un-upgraded driver writes only the legacy prefix (the host's
        /// zero-initialized buffer then reads `0` = unknown/clean), an un-upgraded host retrieves a
        /// legacy-size buffer (the driver writes just the prefix).
        pub cursor_excluded: u32,
    }

    /// [`AddReply`]'s size before the `cursor_excluded` tail — the prefix an un-upgraded driver
    /// writes and an un-upgraded host retrieves (see the field docs).
    pub const ADD_REPLY_LEGACY_SIZE: usize = 20;

    /// `IOCTL_REMOVE` input.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct RemoveRequest {
        pub session_id: u64,
    }

    /// `IOCTL_UPDATE_MODES` input (v4): the live monitor (by its ADD `session_id`) and the new
    /// preferred mode its target-mode list should lead with. The driver replaces the stored list
    /// (new mode first, then its built-in fallbacks — the same shape ADD produces) and pushes it to
    /// the OS via `IddCxMonitorUpdateModes2`; success means the OS accepted the new list, after
    /// which the host force-sets the mode via CCD/GDI as usual.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct UpdateModesRequest {
        pub session_id: u64,
        pub width: u32,
        pub height: u32,
        pub refresh_hz: u32,
        /// Pads the `u64`-aligned struct to a multiple of 8 (Pod forbids implicit tail padding).
        pub _reserved: u32,
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

    /// `IOCTL_SET_FRAME_CHANNEL` input — the sealed frame channel's bootstrap. Every handle field is a
    /// handle VALUE already duplicated into the driver's WUDFHost process by the host. Ownership is
    /// **adopt-on-success-only** (`design/idd-push-security.md` invariant 5): the driver owns (and
    /// eventually closes) the handles IFF it completes the IOCTL successfully — a replaced or
    /// later-unconsumed delivery is then the driver's to close. On ANY error completion (malformed
    /// request, unknown `target_id`) the driver must NOT close them: the HOST reaps its remote
    /// duplicates (`DUPLICATE_CLOSE_SOURCE`). Exactly one side closes each value; a driver that closed
    /// on error would double-close possibly-reused handle values against the host's reap.
    ///
    /// Handle values are only meaningful inside the target process's handle table, so this struct is
    /// harmless to any third party: reading it leaks nothing openable, and spoofing it (were the control
    /// device reachable — it is ACL'd to SYSTEM + admins) could at worst feed the driver values that
    /// don't resolve, a DoS of the attacker's own session. The frame objects themselves are unnamed and
    /// therefore unreachable by any process that isn't one of the two endpoints.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct SetFrameChannelRequest {
        /// The OS target id from [`AddReply`] — which monitor this channel belongs to.
        pub target_id: u32,
        /// The ring generation these textures belong to (must match the shared header's generation at
        /// attach time; a stale delivery is dropped by the driver — a fresh one follows every recreate).
        pub generation: u32,
        /// How many leading entries of `texture_handles` are valid (`1..=`[`RING_LEN`]).
        pub ring_len: u32,
        /// Bytes the host allocated for the shared header section (v3; the former `_pad`, same
        /// offset — a pre-v3 host leaves it 0, which every v3 gate reads as "v2 prefix only").
        /// Together with the header's stamped `version` this is the ONLY permission to touch the
        /// v3 tail: see [`frame::v3_readable`](crate::frame::v3_readable).
        pub header_bytes: u32,
        /// The shared-header file-mapping handle (the driver maps it and writes status/publish tokens).
        pub header_handle: u64,
        /// The frame-ready auto-reset event handle (the driver signals it after each publish).
        pub event_handle: u64,
        /// The ring textures' shared NT handles (opened via `ID3D11Device1::OpenSharedResource1`).
        pub texture_handles: [u64; RING_LEN_USIZE],
    }

    /// [`RING_LEN`] as a usize for the `texture_handles` array length (the wire struct sizes the array
    /// at the compile-time maximum; `ring_len` says how many entries are live).
    pub const RING_LEN_USIZE: usize = RING_LEN as usize;

    /// `IOCTL_SET_CURSOR_CHANNEL` input (v5): the hardware-cursor section for one monitor.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct SetCursorChannelRequest {
        /// The OS target id from [`AddReply`] — which monitor this channel belongs to.
        pub target_id: u32,
        pub _pad: u32,
        /// The [`cursor::CursorShm`](crate::cursor) file-mapping handle VALUE, already duplicated
        /// into the driver's WUDFHost process ([`AddReply::wudf_pid`]).
        pub header_handle: u64,
    }

    /// `IOCTL_SET_CURSOR_FORWARD` input (v6): the mid-stream cursor-render flip for one monitor.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct SetCursorForwardRequest {
        /// The OS target id from [`AddReply`] — which monitor to flip.
        pub target_id: u32,
        /// `1` = declare the hardware cursor (exclude + forward), `0` = un-declare (DWM
        /// composites — the capture mouse model).
        pub enable: u32,
    }

    // Layout is load-bearing across the process boundary — pin it. (bytemuck's Pod derive already
    // rejects any internal padding; these assert the externally-visible sizes too.) The `offset_of!`
    // asserts additionally catch a SAME-SIZE field reorder, which the size+Pod checks alone miss.
    const _: () = {
        use core::mem::{offset_of, size_of};

        assert!(size_of::<AddRequest>() == 40);
        assert!(offset_of!(AddRequest, session_id) == 0);
        assert!(offset_of!(AddRequest, width) == 8);
        assert!(offset_of!(AddRequest, height) == 12);
        assert!(offset_of!(AddRequest, refresh_hz) == 16);
        assert!(offset_of!(AddRequest, preferred_monitor_id) == 20);
        // The client-HDR luminance tail starts exactly at the legacy boundary (prefix-compat).
        assert!(offset_of!(AddRequest, max_luminance_nits) == ADD_REQUEST_LEGACY_SIZE);
        assert!(offset_of!(AddRequest, max_frame_avg_nits) == 28);
        assert!(offset_of!(AddRequest, min_luminance_millinits) == 32);
        // v5: the former tail `_reserved` — same offset, same total size (rename-only).
        assert!(offset_of!(AddRequest, hw_cursor) == 36);
        assert!(size_of::<AddRequest>() == 40);

        assert!(size_of::<AddReply>() == 24);
        assert!(offset_of!(AddReply, adapter_luid_low) == 0);
        assert!(offset_of!(AddReply, adapter_luid_high) == 4);
        assert!(offset_of!(AddReply, target_id) == 8);
        assert!(offset_of!(AddReply, resolved_monitor_id) == 12);
        assert!(offset_of!(AddReply, wudf_pid) == 16);
        // The cursor-excluded tail starts exactly at the legacy boundary (prefix-compat).
        assert!(offset_of!(AddReply, cursor_excluded) == ADD_REPLY_LEGACY_SIZE);

        assert!(size_of::<SetFrameChannelRequest>() == 32 + 8 * RING_LEN_USIZE);
        assert!(offset_of!(SetFrameChannelRequest, target_id) == 0);
        assert!(offset_of!(SetFrameChannelRequest, generation) == 4);
        assert!(offset_of!(SetFrameChannelRequest, ring_len) == 8);
        assert!(offset_of!(SetFrameChannelRequest, header_handle) == 16);
        assert!(offset_of!(SetFrameChannelRequest, event_handle) == 24);
        assert!(offset_of!(SetFrameChannelRequest, texture_handles) == 32);

        assert!(size_of::<RemoveRequest>() == 8);
        assert!(offset_of!(RemoveRequest, session_id) == 0);

        assert!(size_of::<SetCursorChannelRequest>() == 16);
        assert!(offset_of!(SetCursorChannelRequest, target_id) == 0);
        assert!(offset_of!(SetCursorChannelRequest, header_handle) == 8);
        assert!(size_of::<SetCursorForwardRequest>() == 8);
        assert!(offset_of!(SetCursorForwardRequest, target_id) == 0);
        assert!(offset_of!(SetCursorForwardRequest, enable) == 4);

        assert!(size_of::<UpdateModesRequest>() == 24);
        assert!(offset_of!(UpdateModesRequest, session_id) == 0);
        assert!(offset_of!(UpdateModesRequest, width) == 8);
        assert!(offset_of!(UpdateModesRequest, height) == 12);
        assert!(offset_of!(UpdateModesRequest, refresh_hz) == 16);

        assert!(size_of::<SetRenderAdapterRequest>() == 8);
        assert!(offset_of!(SetRenderAdapterRequest, luid_low) == 0);
        assert!(offset_of!(SetRenderAdapterRequest, luid_high) == 4);

        assert!(size_of::<InfoReply>() == 8);
        assert!(offset_of!(InfoReply, protocol_version) == 0);
        assert!(offset_of!(InfoReply, watchdog_timeout_s) == 4);
    };
}

/// CTA-861.3 "Desired Content Luminance" coding for the pf-vdisplay EDID's HDR Static Metadata
/// Data Block — the three bytes that tell Windows (and through it every host app) what luminance
/// volume the virtual display's panel has. The HOST fills [`control::AddRequest`]'s luminance
/// fields from the CLIENT's real display volume and the DRIVER codes them here, so games tone-map
/// to the panel the stream actually lands on.
///
/// Lives in this shared crate (not the driver) deliberately: the driver only builds under the WDK
/// on Windows, but this byte-level coding is exactly the fiddly part that wants unit tests on
/// every dev machine BEFORE a driver build/sign/deploy cycle. `no_std` + integer-only (fixed
/// point), so it drops into the driver unchanged.
pub mod edid {
    /// `2^(k/32)` for `k = 0..32` in Q16 fixed point (`round(2^(k/32) * 65536)`) — the fractional
    /// step table for the CTA-861.3 luminance exponent.
    const POW2_Q16: [u32; 32] = [
        65536, 66971, 68438, 69936, 71468, 73032, 74632, 76266, 77936, 79642, 81386, 83169, 84990,
        86851, 88752, 90696, 92682, 94711, 96785, 98905, 101070, 103283, 105545, 107856, 110218,
        112631, 115098, 117618, 120194, 122825, 125515, 128263,
    ];

    /// Decode a CTA-861.3 max / frame-average luminance code to MILLI-nits:
    /// `L = 50 * 2^(CV/32)` cd/m², so `L_millinits = 50_000 * 2^(CV/32)`.
    /// (`CV = 255` ≈ 12_525 nits — comfortably inside u64 at Q16.)
    pub const fn cta_max_millinits(code: u8) -> u64 {
        let whole = code as u32 / 32;
        let frac = code as u32 % 32;
        ((50_000u64 << whole) * POW2_Q16[frac as usize] as u64) >> 16
    }

    /// Code a display's peak (or frame-average) luminance in nits as a CTA-861.3 luminance value:
    /// the LARGEST code whose decoded luminance does not exceed the panel's — never advertise a
    /// volume brighter than the glass, so a host app's tone map can't clip on the client. Clamped
    /// to `1..=255`: `0` is "no data" on the wire, and callers gate on `nits > 0` themselves (a
    /// sub-51-nit request — no real HDR panel — still codes as 1).
    pub fn cta_max_luminance_code(nits: u32) -> u8 {
        let target = nits as u64 * 1000;
        let mut code = 1u8;
        while code < 255 && cta_max_millinits(code + 1) <= target {
            code += 1;
        }
        code
    }

    /// Floor integer square root (Newton's method — `u64::isqrt` needs Rust 1.84, above this
    /// crate's 1.82 MSRV). Converges in ≤ 6 iterations from the power-of-two seed.
    fn isqrt_u64(x: u64) -> u64 {
        if x == 0 {
            return 0;
        }
        // Seed strictly above sqrt(x): 2^(ceil(bits/2)).
        let mut r = 1u64 << (64 - x.leading_zeros()).div_ceil(2);
        loop {
            let next = (r + x / r) / 2;
            if next >= r {
                return r;
            }
            r = next;
        }
    }

    /// Code a display's min luminance (MILLI-nits) as the CTA-861.3 min-luminance value, which is
    /// relative to the block's coded max: `L_min = L_max * (CV/255)^2 / 100`, so
    /// `CV = 255 * sqrt(100 * L_min / L_max)` — rounded to nearest. `max_code` is the byte
    /// produced by [`cta_max_luminance_code`]; a result of `0` (a true-black panel, or
    /// `millinits = 0` = unknown) is valid on the wire.
    pub fn cta_min_luminance_code(millinits: u32, max_code: u8) -> u8 {
        let max_millinits = cta_max_millinits(max_code);
        if millinits == 0 || max_millinits == 0 {
            return 0;
        }
        // CV = sqrt(100 * 255^2 * L_min / L_max); round to nearest by comparing the two flanking
        // squares (the integer sqrt floors).
        let x = (100u64 * 255 * 255).saturating_mul(millinits as u64) / max_millinits;
        let floor = isqrt_u64(x);
        let cv = if (floor + 1) * (floor + 1) - x <= x - floor * floor {
            floor + 1
        } else {
            floor
        };
        cv.min(255) as u8
    }

    /// Fixed reduced-blanking geometry for [`dtd`] (CVT-RBv2-shaped): 80 px of horizontal and 45
    /// lines of vertical blanking, front-porch/sync splits within them. A virtual display has no
    /// real scan-out, so the blanking only has to be self-consistent — the pixel clock is derived
    /// from these same totals.
    const DTD_H_BLANK: u32 = 80;
    const DTD_V_BLANK: u32 = 45;
    const DTD_H_SYNC_OFFSET: u32 = 8;
    const DTD_H_SYNC_WIDTH: u32 = 32;
    const DTD_V_SYNC_OFFSET: u32 = 3;
    const DTD_V_SYNC_WIDTH: u32 = 5;

    /// An 18-byte EDID detailed timing descriptor for `width`×`height`@`refresh_hz` with the fixed
    /// reduced blanking above — the pf-vdisplay EDID's PREFERRED timing, built from the mode the
    /// session actually asked for instead of a hard-coded 1080p60. `None` when the mode does not
    /// FIT the DTD encoding — pixel clock above 655.35 MHz (the u16 10 kHz field: 4K120-class) or
    /// active dimensions above the 12-bit fields — in which case the caller keeps its
    /// representable fallback descriptor. Flags byte: digital separate sync, +H/+V (0x1E, matching
    /// the previous hard-coded DTD).
    pub fn dtd(width: u32, height: u32, refresh_hz: u32) -> Option<[u8; 18]> {
        if width == 0 || height == 0 || refresh_hz == 0 || width > 4095 || height > 4095 {
            return None;
        }
        let h_total = u64::from(width + DTD_H_BLANK);
        let v_total = u64::from(height + DTD_V_BLANK);
        let clock_10khz = h_total * v_total * u64::from(refresh_hz) / 10_000;
        let clock_10khz = u16::try_from(clock_10khz).ok()?;
        let mut d = [0u8; 18];
        d[0..2].copy_from_slice(&clock_10khz.to_le_bytes());
        d[2] = (width & 0xFF) as u8;
        d[3] = (DTD_H_BLANK & 0xFF) as u8;
        d[4] = (((width >> 8) & 0x0F) << 4) as u8 | ((DTD_H_BLANK >> 8) & 0x0F) as u8;
        d[5] = (height & 0xFF) as u8;
        d[6] = (DTD_V_BLANK & 0xFF) as u8;
        d[7] = (((height >> 8) & 0x0F) << 4) as u8 | ((DTD_V_BLANK >> 8) & 0x0F) as u8;
        d[8] = (DTD_H_SYNC_OFFSET & 0xFF) as u8;
        d[9] = (DTD_H_SYNC_WIDTH & 0xFF) as u8;
        d[10] = (((DTD_V_SYNC_OFFSET & 0x0F) << 4) | (DTD_V_SYNC_WIDTH & 0x0F)) as u8;
        d[11] = ((((DTD_H_SYNC_OFFSET >> 8) & 0x03) << 6)
            | (((DTD_H_SYNC_WIDTH >> 8) & 0x03) << 4)
            | (((DTD_V_SYNC_OFFSET >> 4) & 0x03) << 2)
            | ((DTD_V_SYNC_WIDTH >> 4) & 0x03)) as u8;
        // Bytes 12..17 (image size mm, borders) stay 0 = undefined, matching the previous DTD.
        d[17] = 0x1E;
        Some(d)
    }
}

/// The IDD-push frame transport: the host-created shared ring header, the publish token, and the
/// driver-status codes. The texture ring itself is host-created **unnamed** D3D11 keyed-mutex textures;
/// the driver reaches them (and the header + event) only through handles the host duplicated into its
/// process and delivered via [`crate::control::IOCTL_SET_FRAME_CHANNEL`] — the sealed channel. Only the
/// *layout/contract* lives here.
pub mod frame {
    use bytemuck::{Pod, Zeroable};

    /// Header magic (`"PFVD"` LE). The host stamps it LAST (after the ring textures exist) so the driver
    /// only attaches to a fully-published ring.
    pub const MAGIC: u32 = 0x4456_4650;
    /// Frame-plane version (independent bump of the header layout). v2 appended the stall-attribution
    /// telemetry tail (`drain_heartbeat_qpc`/`last_acquire_qpc`/`offered_total`); v3 appended the
    /// ring-health tail (state, capabilities, epochs, source sequence, terminal error, counters) —
    /// see [`VERSION_TELEMETRY`] and [`VERSION_HEALTH`] for the two compatibility gates.
    pub const VERSION: u32 = 3;
    /// The header version that grew the ring-health tail (immunity plan WP4). Reading or writing
    /// past [`HEADER_V2_SIZE`] needs BOTH gates — see [`v3_readable`]: the host-stamped `version`
    /// says the layout exists, the delivery's `header_bytes` says the section is that large. A v2
    /// section is never touched past byte 88; a v3 host may allocate the larger section for a v2
    /// driver, which reads only the prefix and writes no v3 field.
    pub const VERSION_HEALTH: u32 = 3;
    /// The v2 (telemetry-tail) header size — the prefix every driver since v2 understands.
    pub const HEADER_V2_SIZE: usize = 88;
    /// The v3 (ring-health-tail) header size — the section a v3 host allocates.
    pub const HEADER_V3_SIZE: usize = 152;

    /// Both endpoints may read/write the v3 tail only when the host stamped a v3 layout AND the
    /// delivery declared a section at least that large. One gate, shared, so neither side can
    /// guess: an old host leaves `header_bytes` zero (its request `_pad`), which fails here.
    #[must_use]
    pub const fn v3_readable(version: u32, header_bytes: u32) -> bool {
        version >= VERSION_HEALTH && header_bytes as usize >= HEADER_V3_SIZE
    }

    // Capability bits, stamped by each side into its own header word (`host_capabilities` /
    // `capabilities`). A transport or actuator activates only where BOTH sides agree
    // ([`negotiate`]); a mismatch never selects a protocol by guesswork.
    /// Understands the v3 ring-health tail.
    pub const CAP_RING_HEALTH_V3: u32 = 1 << 0;
    /// The driver keeps the ring endpoint across a swap-chain assignment (WP5).
    pub const CAP_ENDPOINT_SURVIVES_ASSIGNMENT: u32 = 1 << 1;
    /// CAS + shared-fence slot transport (WP7).
    pub const CAP_FENCE_RING: u32 = 1 << 2;
    /// The driver accepts a swap-chain reset actuator (WP13).
    pub const CAP_SWAPCHAIN_RESET: u32 = 1 << 3;
    /// The driver stamps `source_sequence` and `qpc_pts` (source present QPC) per real frame.
    pub const CAP_SOURCE_SEQUENCE_QPC: u32 = 1 << 4;

    /// The capabilities both sides advertise — the only ones either may act on.
    #[must_use]
    pub const fn negotiate(host: u32, driver: u32) -> u32 {
        host & driver
    }

    /// `SharedHeader::health_state` values (v3). The driver stores the state LAST, with Release,
    /// after the epoch/sequence fields it describes; a reader loads it with Acquire before
    /// trusting them, and re-reads it after to detect a torn snapshot ([`snapshot_consistent`]).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u32)]
    pub enum HealthState {
        /// Header created, no publisher attached yet (also what a pre-v3 driver leaves: 0).
        Initializing = 0,
        /// A publisher is attached and the ring is live.
        Active = 1,
        /// The publisher retired its attachment (generation superseded / worker exit); a fresh
        /// attach is expected.
        Rebuilding = 2,
        /// The generation is poisoned (abandoned slot, fatal sync/device result). `terminal_error_*`
        /// name the cause. Only a rebuild helps.
        Dead = 3,
    }

    impl HealthState {
        /// Decode a stored word; unknown values read as [`Self::Dead`] — an unrecognised state is
        /// not one to keep streaming on.
        #[must_use]
        pub const fn from_u32(v: u32) -> Self {
            match v {
                0 => Self::Initializing,
                1 => Self::Active,
                2 => Self::Rebuilding,
                _ => Self::Dead,
            }
        }
    }

    /// `terminal_error_domain` values (v3), paired with a raw `terminal_error_code`.
    pub const ERR_DOMAIN_NONE: u32 = 0;
    /// Slot synchronization: an abandoned or failed keyed-mutex/fence operation (code = HRESULT).
    pub const ERR_DOMAIN_TRANSPORT: u32 = 1;
    /// The D3D device: removed/reset (code = `GetDeviceRemovedReason`).
    pub const ERR_DOMAIN_DEVICE: u32 = 2;

    /// Torn-read resistance for a v3 snapshot: the state word read before and after the epoch/
    /// sequence fields must match, or the snapshot spans a driver transition and is discarded.
    #[must_use]
    pub const fn snapshot_consistent(state_before: u32, state_after: u32) -> bool {
        state_before == state_after
    }
    /// The header version that grew the telemetry tail. Compatibility is gated on the HOST-stamped
    /// `version` field, not on mapping sizes: a v2 driver writes the tail only when
    /// `version >= VERSION_TELEMETRY` (a v1 host created a 64-byte layout — never write past it),
    /// and a v2 host reads `drain_heartbeat_qpc == 0` as "pre-telemetry driver, no verdict" (the
    /// same zero-means-absent convention as [`OPENED_DETAIL_LIVE`]). Neither side rejects the
    /// other's version — the tail is diagnostics, not frame-plane semantics.
    pub const VERSION_TELEMETRY: u32 = 2;
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
    /// Driver refused the attach because the mapped ring names a DIFFERENT monitor
    /// ([`SharedHeader::target_id`] != the monitor the delivery landed on) — a host stash cross-wire
    /// or stale-delivery race that, with parallel displays, would carry one client's frames into
    /// another client's stream. Fail-closed binding validation (v3, invariant #10 of
    /// `design/idd-push-security.md`); `driver_status_detail` carries the target id the ring claims.
    pub const DRV_STATUS_BIND_FAIL: u32 = 4;

    /// While `driver_status` is [`DRV_STATUS_OPENED`], `driver_status_detail` carries a LIVE
    /// diagnostic word maintained by the driver's publisher (best-effort plain writes — the same
    /// visibility contract as `driver_status` itself). Layout:
    ///
    /// - bit 31 (this constant): stamped at attach by a detail-capable driver, so the host can
    ///   tell "pre-detail driver, no information" (field = 0) from "zero frames offered".
    /// - bits 30..16: surfaces the swap-chain worker OFFERED to the ring (15-bit, saturating) —
    ///   every DWM compose that reached `publish()`, whatever its outcome.
    /// - bits 15..0: publishes DROPPED for a descriptor mismatch (16-bit, saturating) — the
    ///   surface's size/format didn't match the ring's.
    ///
    /// The host's wait-for-attach reads this on its first-frame timeout to NAME the failure
    /// instead of guessing (the lid-closed field report was undiagnosable without it):
    /// `offered == 0` → DWM never composed the display (powered-off / undamaged desktop, compose
    /// kicks blocked); `offered > 0` with `seq` still 0 → every compose was dropped mismatched
    /// (the host sized the ring from a stale or foreign-session GDI mode).
    pub const OPENED_DETAIL_LIVE: u32 = 0x8000_0000;

    /// Pack the live OPENED diagnostic word (see [`OPENED_DETAIL_LIVE`]); both counters saturate.
    #[must_use]
    pub const fn pack_opened_detail(offered: u32, mismatched: u32) -> u32 {
        let o = if offered > 0x7FFF { 0x7FFF } else { offered };
        let m = if mismatched > 0xFFFF {
            0xFFFF
        } else {
            mismatched
        };
        OPENED_DETAIL_LIVE | (o << 16) | m
    }

    /// Unpack a live OPENED diagnostic word → `(offered, mismatched)`; `None` when the driver
    /// never stamped [`OPENED_DETAIL_LIVE`] (a pre-detail driver — the field carries nothing).
    #[must_use]
    pub const fn unpack_opened_detail(detail: u32) -> Option<(u32, u32)> {
        if detail & OPENED_DETAIL_LIVE == 0 {
            return None;
        }
        Some(((detail >> 16) & 0x7FFF, detail & 0xFFFF))
    }

    /// The shared metadata header (host-created, mapped by both sides). Atomic fields (`magic`, `latest`,
    /// `generation`) are accessed via each side's own atomic view over the mapping; this is the layout.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug)]
    pub struct SharedHeader {
        pub magic: u32,
        pub version: u32,
        /// Bumped by the host on a ring recreate (HDR-mode flip → new texture format + a fresh
        /// [`control::IOCTL_SET_FRAME_CHANNEL`](crate::control::IOCTL_SET_FRAME_CHANNEL) delivery). The
        /// driver re-attaches when it changes; a publish carries it so the host rejects a stale-ring
        /// publish.
        pub generation: u32,
        pub ring_len: u32,
        pub width: u32,
        pub height: u32,
        pub dxgi_format: u32,
        /// The OS target id of the monitor this ring belongs to (v3 — the former `_pad`, same
        /// offset). Host-stamped at ring creation, BEFORE the magic (the magic-last publish ordering
        /// guarantees the driver never reads it half-initialized) and never changed afterwards (a
        /// mid-session recreate reuses the mapping, so the binding is stable for the ring's life).
        /// The driver's publisher attaches only when it equals the monitor's own target id
        /// ([`check_attach`]) — a mis-delivered ring fails closed ([`DRV_STATUS_BIND_FAIL`]) instead
        /// of carrying another display's frames (invariant #10, `design/idd-push-security.md`).
        pub target_id: u32,
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
        /// v2 telemetry tail (stall attribution, Phase A.1 of the vdisplay-disturbance-immunity
        /// program): driver-written on every drain-loop pass, host-read when a capture stall ends.
        /// All three are best-effort Relaxed atomic stores (the `driver_status` visibility
        /// contract); `0` means a pre-v2 driver never wrote them. QPC of the swap-chain worker's
        /// most recent drain-loop iteration — E_PENDING passes included, so a fresh heartbeat over
        /// a stale [`Self::last_acquire_qpc`] reads "the worker is running and DWM is composing
        /// nothing", while a stale heartbeat reads "our worker starved".
        pub drain_heartbeat_qpc: u64,
        /// QPC of the most recent SUCCESSFUL swap-chain acquire — the last instant DWM actually
        /// composed this display. The Branch-1/Branch-2 fork in one field.
        pub last_acquire_qpc: u64,
        /// Wrapping count of surfaces offered to the ring publisher — the full-width sibling of
        /// the packed 15-bit [`OPENED_DETAIL_LIVE`] counter (which saturates and cannot be
        /// delta'd over a stall window). The host snapshots it per consumed frame; the delta
        /// across a stall says whether frames existed that never reached the ring.
        pub offered_total: u64,
        // ---- v3 ring-health tail (offset 88..152). Every write is gated on `v3_readable`. ----
        /// [`HealthState`], driver-written with Release AFTER the fields below it describes.
        pub health_state: u32,
        /// Driver capability bits (`CAP_*`), stamped at attach.
        pub capabilities: u32,
        /// Host capability bits (`CAP_*`), stamped before the magic.
        pub host_capabilities: u32,
        /// Bumped by the driver on every swap-chain assignment it attaches under.
        pub assignment_epoch: u32,
        /// Bumped by the driver on every D3D device creation — even on the same LUID (a TDR
        /// recreate mints a new epoch; LUID equality is never device-compatibility proof).
        pub device_epoch: u32,
        /// `ERR_DOMAIN_*` for a [`HealthState::Dead`] generation.
        pub terminal_error_domain: u32,
        /// Raw code for `terminal_error_domain` (HRESULT / removed reason).
        pub terminal_error_code: i32,
        pub _pad_v3: u32,
        /// Monotonic count of NEW source frames published (a stash republish does not advance it)
        /// — the driver-side twin of the host's provenance sequence.
        pub source_sequence: u64,
        /// QPC of the most recent successful publish.
        pub last_publish_qpc: u64,
        /// Wrapping counts of publishes that landed and frames dropped (busy/mismatch/fatal).
        pub published_total: u64,
        pub dropped_total: u64,
    }

    /// Why the driver's publisher must NOT attach a delivered channel to its monitor's ring — the
    /// two reject outcomes of [`check_attach`], each with different driver behavior.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum AttachReject {
        /// The header isn't (or is no longer) the ring this delivery described: magic missing, or
        /// the host recreated the ring again before the attach (a fresh delivery is on its way).
        /// Benign — drop the delivery silently; no status is written.
        Stale,
        /// The ring names a DIFFERENT monitor (`SharedHeader::target_id` mismatch) — a host
        /// stash/delivery cross-wire that, with parallel displays, would publish this monitor's
        /// frames into another client's stream. Fail closed: refuse the attach and write
        /// [`DRV_STATUS_BIND_FAIL`] so the host's wait-for-attach fails the open loudly.
        BindMismatch,
    }

    /// The publisher's attach precondition (v3): given the mapped header's `magic`, `generation`
    /// and `target_id` plus the delivery's generation and the monitor's own target id, decide
    /// whether the attach may proceed. Staleness is checked FIRST — a superseded delivery's binding
    /// is meaningless (the fresh delivery re-validates it), so it never false-alarms as a bind
    /// failure. Pure and shared-crate-owned so the reject paths are unit-tested on every dev
    /// machine (the driver workspace builds `panic = "abort"` and cannot host a test harness).
    pub fn check_attach(
        magic: u32,
        header_generation: u32,
        header_target_id: u32,
        delivery_generation: u32,
        monitor_target_id: u32,
    ) -> Result<(), AttachReject> {
        if magic != MAGIC || header_generation != delivery_generation {
            return Err(AttachReject::Stale);
        }
        if header_target_id != monitor_target_id {
            return Err(AttachReject::BindMismatch);
        }
        Ok(())
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

    // Size + per-field offsets are load-bearing: both sides access these via raw atomic views over the
    // mapping, so a same-size field reorder would silently corrupt. Pin every offset. `target_id`
    // (v3, the former `_pad`) after `dxgi_format` is what 8-aligns the `u64 latest` at offset 32 —
    // assert that too.
    const _: () = {
        use core::mem::{offset_of, size_of};

        assert!(size_of::<SharedHeader>() == HEADER_V3_SIZE);
        assert!(offset_of!(SharedHeader, health_state) == HEADER_V2_SIZE);
        assert!(offset_of!(SharedHeader, capabilities) == 92);
        assert!(offset_of!(SharedHeader, host_capabilities) == 96);
        assert!(offset_of!(SharedHeader, assignment_epoch) == 100);
        assert!(offset_of!(SharedHeader, device_epoch) == 104);
        assert!(offset_of!(SharedHeader, terminal_error_domain) == 108);
        assert!(offset_of!(SharedHeader, terminal_error_code) == 112);
        assert!(offset_of!(SharedHeader, source_sequence) == 120);
        assert!(offset_of!(SharedHeader, last_publish_qpc) == 128);
        assert!(offset_of!(SharedHeader, published_total) == 136);
        assert!(offset_of!(SharedHeader, dropped_total) == 144);
        assert!(offset_of!(SharedHeader, magic) == 0);
        assert!(offset_of!(SharedHeader, version) == 4);
        assert!(offset_of!(SharedHeader, generation) == 8);
        assert!(offset_of!(SharedHeader, ring_len) == 12);
        assert!(offset_of!(SharedHeader, width) == 16);
        assert!(offset_of!(SharedHeader, height) == 20);
        assert!(offset_of!(SharedHeader, dxgi_format) == 24);
        assert!(offset_of!(SharedHeader, target_id) == 28);
        assert!(offset_of!(SharedHeader, latest) == 32);
        assert!(offset_of!(SharedHeader, qpc_pts) == 40);
        assert!(offset_of!(SharedHeader, driver_render_luid_low) == 48);
        assert!(offset_of!(SharedHeader, driver_render_luid_high) == 52);
        assert!(offset_of!(SharedHeader, driver_status) == 56);
        assert!(offset_of!(SharedHeader, driver_status_detail) == 60);
        assert!(offset_of!(SharedHeader, drain_heartbeat_qpc) == 64);
        assert!(offset_of!(SharedHeader, last_acquire_qpc) == 72);
        assert!(offset_of!(SharedHeader, offered_total) == 80);
    };
}

/// Gamepad shared-memory layouts (host ↔ the UMDF gamepad drivers `pf_xusb` / `pf_gamepad`).
///
/// These were hand-duplicated as `OFF_*`/`SHM_*` constants in `inject/{gamepad,dualsense}_windows.rs`
/// and (as bare literals — `*view.add(140)`) in the standalone `xusb-driver`/`dualsense-driver`
/// workspaces, guarded only by "must match" comments — the top ABI-drift hazard the audit flagged
/// (`design/windows-host-rewrite.md` §2.7). Owning them here with `Pod` derives + `offset_of!`
/// asserts makes a one-sided edit a compile error.
///
/// Since v2 the channel is **sealed** (`design/gamepad-channel-sealing.md`, mirroring the frame
/// channel): the host creates the DATA section ([`XusbShm`]/[`PadShm`]) UNNAMED (SYSTEM-only DACL)
/// and duplicates its handle into the driver's WUDFHost; only the tiny [`PadBootstrap`] mailbox
/// stays named (it carries nothing exploitable). Layout only; the sections are host-created.
pub mod gamepad {
    use alloc::string::String;
    use bytemuck::{Pod, Zeroable};

    /// XUSB section magic — the exact u32 the shipped host + `pf_xusb` driver compare (loosely "PFXU").
    pub const XUSB_MAGIC: u32 = 0x5558_4650;
    /// Pad section magic — the exact u32 the shipped host + `pf_gamepad` driver compare (loosely
    /// "PFDS"). (Note: the two magics happen to use opposite byte-order mnemonics in the legacy code;
    /// only the u32 value is the contract.)
    pub const PAD_MAGIC: u32 = 0x5046_4453;

    /// `device_type` selector the `pf_gamepad` driver reads to pick its HID identity. The section is
    /// zeroed, so `0` = DualSense is the default; one driver serves every identity.
    pub const DEVTYPE_DUALSENSE: u8 = 0;
    /// `device_type` = DualShock 4 (`VID_054C&PID_09CC` HID identity).
    pub const DEVTYPE_DUALSHOCK4: u8 = 1;
    /// `device_type` = DualSense Edge (`VID_054C&PID_0DF2` HID identity — the DualSense report
    /// codec plus the four native back/Fn button bits).
    pub const DEVTYPE_DUALSENSE_EDGE: u8 = 2;
    /// `device_type` = Steam Deck controller (`VID_28DE&PID_1205` HID identity, the captured
    /// controller-interface descriptor + the Steam `0x83`/`0xAE` feature contract). Promoted by
    /// Steam Input on Windows when the devnode's synthesized USB hardware ids carry `&MI_02`
    /// (the wired controller interface — the N4-spike finding).
    pub const DEVTYPE_STEAMDECK: u8 = 3;
    /// `device_type` = Xbox Wireless Controller (`VID_045E&PID_0B13` HID identity — a Bluetooth
    /// Xbox pad, which unlike the wired `045E:028E`/`045E:02EA` ids IS a real HID device).
    ///
    /// This exists because the OTHER Windows Xbox backend, `pf-xusb`, registers only
    /// `GUID_DEVINTERFACE_XUSB` and has no HID collection — so Steam, WGI, GameInput, DirectInput
    /// and `joy.cpl` cannot enumerate it at all, and only classic `XInputGetState` ever sees it
    /// (field 2026-08-09). Routing an Xbox pad through this identity instead puts it on the same
    /// HID footing the PlayStation pads have always had.
    ///
    /// ⚠️ Unlike its siblings the Xbox input report is NOT 64 bytes — it is
    /// `XBOX_INPUT_REPORT_LEN` (16). The driver serves per-identity report lengths because
    /// hidclass sizes its buffer from the descriptor and refuses an over-long source.
    pub const DEVTYPE_XBOX: u8 = 4;
    /// `device_type` = Xbox One S controller over Bluetooth (`VID_045E&PID_02FD`).
    ///
    /// ⭐ **Shares [`DEVTYPE_XBOX`]'s report descriptor, byte for byte.** All three Xbox identities
    /// are the same pad in HID terms — same axes, same trigger pair, same hat, same 15 buttons,
    /// same rumble output report — and differ ONLY in VID/PID, product string and INF model line.
    /// The descriptor is the report SHAPE; the identity is what the OS keys mappings off. Giving
    /// each identity its own hand-written descriptor would triple a debt that has already cost
    /// three separate bugs (see the `XBOX_RDESC` provenance block in the driver).
    pub const DEVTYPE_XBOX_ONE_S: u8 = 5;
    /// `device_type` = Xbox Elite Wireless Controller Series 2 (`VID_045E&PID_0B22`) — the
    /// hardware `tools/hid-descriptor-dump` captured on `.173`.
    ///
    /// ⚠️ The four paddles are NOT in this identity's report yet. See [`DEVTYPE_XBOX_ONE_S`] for
    /// why the descriptor is shared, and `design/xbox-pad-windows-handoff.md` §4 WP-C for the
    /// unresolved tension: once the pad is promoted, `xinputhid` claims the HID collection
    /// exclusively, so extra buttons declared here may be invisible to every consumer anyway.
    /// That needs measuring before it is built.
    pub const DEVTYPE_XBOX_ELITE: u8 = 6;
    /// Steam Controller 2 ("Triton", 2026): wired identity `28DE:1302`. Raw-passthrough pad —
    /// the host feeds the client's captured reports verbatim and the driver answers Steam's
    /// feature query-dance in-driver (see the `triton` module).
    pub const DEVTYPE_TRITON: u8 = 7;

    /// The value a gamepad driver writes into its section's `driver_proto` field once it attaches —
    /// the host's positive "driver is alive on this section" signal (health check + version audit).
    /// The section starts zeroed, so `0` always means "no driver has attached (yet)"; a pre-health
    /// driver never writes the field and reads as not-attached, which the host log line calls out
    /// (the remedy is the same: reinstall the drivers). Bump on a gamepad-layout change.
    ///
    /// v2: the **sealed pad channel** (`design/gamepad-channel-sealing.md`) — the DATA section
    /// ([`XusbShm`]/[`PadShm`]) is UNNAMED and reaches the driver only as a handle the host duplicated
    /// into its WUDFHost, bootstrapped through the named [`PadBootstrap`] mailbox; the DATA section
    /// gained `pad_index` (carved from reserved space) so the driver rejects a cross-pad delivery.
    /// A v1 driver opens `Global\pf…-shm-<i>` (which no longer exists) and a v1 host never creates
    /// the mailbox a v2 driver polls, so a mixed pairing fails closed either way.
    ///
    /// v3: the **channel proof** ([`ChannelProof`]) — the host stopped trusting the mailbox's
    /// `driver_pid` and now learns the duplication target over the DEVICE STACK instead. A v2 driver
    /// answers no proof, so a v3 host refuses to deliver to it; a v2 host never asks, and a v3 driver
    /// refuses the v2 handshake on the `host_proto` check. Mixed pairings fail closed both ways, with
    /// the existing "update host + drivers together" diagnostic.
    pub const GAMEPAD_PROTO_VERSION: u32 = 3;

    // ── the channel proof (v3): who to hand the DATA section to ──────────────────────────────────
    //
    // WHY THIS EXISTS. Through v2 the host took the duplication target from the mailbox's
    // `driver_pid`. The mailbox has to be openable by LocalService (that is what the driver's own
    // WUDFHost runs as), and the delivery gate — `verify_is_wudfhost` — only checks that the named
    // process's IMAGE is `%SystemRoot%\System32\WUDFHost.exe`, which is world-executable. So any
    // LocalService principal, notably the deliberately de-privileged plugin runner, could spawn its
    // own WUDFHost, publish that pid, and be handed the pad's DATA section: forged HID input into the
    // interactive desktop (the mouse section drives a real absolute pointer) and a read of the remote
    // user's controller state (security-review 2026-07-28).
    //
    // WHY IT HAS TO COME FROM THE DEVICE STACK. That race cannot be closed on the host side alone.
    // Everything the real driver can read at LocalService — the devnode's Location, its
    // `Device Parameters` key, the object namespace — an attacker at LocalService can read too, so no
    // host-published secret tells the two apart. The ONE thing an attacker cannot forge is *being the
    // driver bound to our devnode*: only that process answers I/O sent to the device the host itself
    // created (and the host looks the device up by the instance id `SwDeviceCreate` handed back, so a
    // planted look-alike devnode is not in the running). Asking the devnode "which process are you?"
    // therefore yields a pid the host can trust, and the mailbox is demoted to what it always should
    // have been: a rendezvous for a handle VALUE that is meaningless anywhere but in that process.
    //
    // Two transports, because the drivers are two different shapes:
    //   * `pf_xusb` is a plain UMDF2 driver that owns `GUID_DEVINTERFACE_XUSB` and dispatches its own
    //     IOCTLs -> [`IOCTL_PF_XUSB_GET_CHANNEL_PROOF`].
    //   * `pf_gamepad` / `pf_mouse` are HID minidrivers with no control device (hidclass owns the
    //     stack, and UMDF has no control-device objects), so the reachable read path is a HID string
    //     -> [`HID_STRING_INDEX_CHANNEL_PROOF`], which needs no report-descriptor change. That
    //     matters: the pads' descriptors, VID/PID and serials are what Steam and SDL fingerprint,
    //     and a new feature report there would risk the identity work this driver exists to get right.

    /// Proof magic ("PFCP" — punktfunk channel proof), and the `PFCP` prefix of the text form.
    pub const PROOF_MAGIC: u32 = 0x5043_4650;

    /// Reserved HID string index the `pf_gamepad` / `pf_mouse` minidrivers answer with their
    /// [`ChannelProof`], fetched by the host with `HidD_GetIndexedString`.
    ///
    /// ⚠️ MEASURED UNUSABLE on .173 (Win11 26200): hidclass does not carry an arbitrary indexed-string
    /// request to a UMDF HID minidriver — `HidD_GetIndexedString` failed for EVERY index, including
    /// ones the driver demonstrably serves through the named wrappers. Kept because it costs one
    /// failed IOCTL and is the right thing to ask first if a later Windows starts forwarding it; the
    /// transports that actually work are [`PF_PAD_CONTROL_INTERFACE_GUID_U128`] (if hidclass lets it
    /// through) and, for `pf_mouse`, the serial string ([`proof_is_serial_string`]).
    ///
    /// 16-bit on purpose: both `IOCTL_HID_GET_INDEXED_STRING` and `IOCTL_HID_GET_STRING` pack their
    /// argument as `(language_id << 16) | string_index`, so only the low word survives the trip and
    /// both drivers mask before comparing. `0x5046` ("PF") is still far outside the 1..=255 range a
    /// real USB/HID string-descriptor index can occupy, so it cannot collide with a string the OS, a
    /// game, or Steam asks for.
    pub const HID_STRING_INDEX_CHANNEL_PROOF: u32 = 0x5046;

    // ❌ A private device interface (`WdfDeviceCreateDeviceInterface`) was tried here as a
    // hidclass-independent transport for the HID minidrivers and MEASURED DEAD on .173 (Win11
    // 26200): it registers and enumerates, but `CreateFile` on it is refused (ERROR_GEN_FAILURE)
    // because hidclass owns `IRP_MJ_CREATE` on a devnode it is the FDO for. Do not re-try it for
    // `pf_gamepad`/`pf_mouse`; see `pf_umdf_util::hid` for the full measurement.

    /// The proof question itself, on whichever interface carries it.
    /// `CTL_CODE(0x8000, 0x0FE0, METHOD_BUFFERED, FILE_ANY_ACCESS)`: a function code no xusb22 IOCTL
    /// uses, `METHOD_BUFFERED` so the answer is a plain buffer copy, and `FILE_ANY_ACCESS` so the host
    /// can ask over a `CreateFile` handle opened with NO access rights (the same way it must open a
    /// HID collection). Answering it leaks nothing — a pid is not a secret, and the proof is only
    /// worth anything to a process that can already duplicate handles.
    pub const IOCTL_PF_GET_CHANNEL_PROOF: u32 = 0x8000_3F80;

    /// Whether a driver serves its channel proof AS its HID serial-number string.
    ///
    /// The one transport measured to work against a UMDF HID minidriver today: on .173,
    /// `HidD_GetSerialNumberString` succeeds on a zero-access handle and returns the driver's own
    /// text, so a proof placed there reaches the host. Enabled for **`pf_mouse` only** — its serial
    /// (`PFMOUSE00`) is inert, whereas the pads' serials are what SDL and Steam dedup controllers on,
    /// and Steam is already known to mangle a pad's displayed name over serial FORMAT alone.
    ///
    /// `pf_mouse` is also the one that matters most: its section drives a real absolute pointer, so a
    /// hijacked mouse channel is desktop control, where a hijacked pad channel is gamepad input.
    pub const fn proof_is_serial_string(pad_kind_is_mouse: bool) -> bool {
        pad_kind_is_mouse
    }

    /// The feature report the **PS pad identities** (DualSense / DualShock 4 / Edge) answer the
    /// channel proof on.
    ///
    /// `0x85` is already DECLARED as a Feature report in all three captured descriptors and was
    /// previously unserved — the driver failed it with `STATUS_INVALID_PARAMETER`. That is what makes
    /// this transport free: **no report-descriptor change**, so the VID/PID, report layout, serial and
    /// product strings that Steam and SDL fingerprint are untouched, and `HidD_GetFeature` is allowed
    /// through by hidclass because the id is in the descriptor. Nothing can have depended on the old
    /// failure: SDL reads `0x05`/`0x09`/`0x20` (DualSense) and `0x02`/`0x12`/`0xA3` (DS4); `0x85` is
    /// one of Sony's vendor reports neither it nor Steam asks for.
    pub const HID_FEATURE_REPORT_CHANNEL_PROOF: u8 = 0x85;

    /// The Steam Deck identity's private proof command.
    ///
    /// The Deck descriptor declares ONE unnumbered feature report and Steam drives it as a
    /// command/response protocol (`0x83` GET_ATTRIBUTES, `0xAE` GET_STRING_ATTRIBUTE); the driver
    /// echoes commands it doesn't know. So the proof rides that same contract — SET_FEATURE this
    /// command, then GET_FEATURE the reply — again with no descriptor change. TWO bytes, not one, so
    /// a Steam command byte we haven't catalogued can never be mistaken for it.
    pub const DECK_PROOF_CMD: [u8; 2] = [0xF9, 0x50];

    /// What a driver answers when the host asks, over the device stack, who it is.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct ChannelProof {
        /// [`PROOF_MAGIC`].
        pub magic: u32,
        /// The driver's [`GAMEPAD_PROTO_VERSION`].
        pub proto: u32,
        /// The pad index the driver read from its devnode Location — cross-checked against the pad
        /// the host is delivering, so a mis-resolved devnode can't cross-wire two pads.
        pub pad_index: u32,
        /// `GetCurrentProcessId()` of the driver's WUDFHost: the duplication target.
        pub wudf_pid: u32,
    }

    impl ChannelProof {
        /// This driver's answer. `pad_index` comes from the devnode Location, `wudf_pid` from
        /// `GetCurrentProcessId()`.
        pub fn new(pad_index: u32, wudf_pid: u32) -> ChannelProof {
            ChannelProof {
                magic: PROOF_MAGIC,
                proto: GAMEPAD_PROTO_VERSION,
                pad_index,
                wudf_pid,
            }
        }

        /// Validate an answer against the pad the host is actually delivering, yielding the pid to
        /// duplicate into. `Err` carries the operator-facing reason — every rejection is a refusal to
        /// deliver, so the host must be able to say precisely which check failed rather than falling
        /// back to a pid it cannot trust.
        pub fn check(&self, expect_pad_index: u32) -> Result<u32, &'static str> {
            if self.magic != PROOF_MAGIC {
                return Err(
                    "the devnode's answer is not a punktfunk channel proof (bad magic) — \
                            some other driver is bound to this device",
                );
            }
            if self.proto != GAMEPAD_PROTO_VERSION {
                return Err(
                    "the driver bound to this devnode speaks a different gamepad protocol \
                            — update the host and the drivers together",
                );
            }
            if self.pad_index != expect_pad_index {
                return Err(
                    "the devnode answered for a DIFFERENT pad index — the interface lookup \
                            resolved the wrong device",
                );
            }
            if self.wudf_pid == 0 {
                return Err("the driver reported pid 0");
            }
            Ok(self.wudf_pid)
        }

        /// The 16 wire bytes of the `pf_xusb` IOCTL answer. Offered here (rather than leaving each
        /// side to reach for `bytemuck`) so the driver crates need no extra dependency and both
        /// sides go through one length-checked pair with [`from_bytes`](Self::from_bytes).
        pub fn to_bytes(self) -> [u8; 16] {
            let mut out = [0u8; 16];
            out.copy_from_slice(bytemuck::bytes_of(&self));
            out
        }

        /// Parse [`to_bytes`](Self::to_bytes). `None` if the device returned fewer bytes than a whole
        /// proof — a short read must refuse the delivery, never be zero-extended into a pid.
        ///
        /// `pod_read_unaligned`, NOT `from_bytes`: the feature-report form offsets the proof by one
        /// byte (the report id sits at 0), so the slice is not 4-aligned and `from_bytes` panics on
        /// it. Device I/O buffers carry no alignment guarantee either.
        pub fn from_bytes(b: &[u8]) -> Option<ChannelProof> {
            (b.len() >= 16).then(|| bytemuck::pod_read_unaligned::<ChannelProof>(&b[..16]))
        }

        /// The proof as a HID **feature report** of exactly `len` bytes: `[report_id, proof(16), 0…]`.
        /// A HID feature reply carries its report id in byte 0 and is sized by the descriptor, so the
        /// driver pads to whatever length the caller's buffer declares. `None` if `len` cannot hold
        /// the id plus a whole proof.
        pub fn to_feature_report(self, report_id: u8, len: usize) -> Option<alloc::vec::Vec<u8>> {
            if len < 17 {
                return None;
            }
            let mut out = alloc::vec![0u8; len];
            out[0] = report_id;
            out[1..17].copy_from_slice(&self.to_bytes());
            Some(out)
        }

        /// Parse [`to_feature_report`](Self::to_feature_report) — skips the leading report id.
        pub fn from_feature_report(b: &[u8]) -> Option<ChannelProof> {
            Self::from_bytes(b.get(1..)?)
        }

        /// Render as the ASCII text a HID indexed-string answer carries:
        /// `PFCP:<proto>:<pad_index>:<wudf_pid>`. Text rather than the raw struct because
        /// `HidD_GetIndexedString` is a string channel, and because a human reading a driver log or
        /// poking the device with a HID inspector should be able to see what the pad answered.
        pub fn to_hid_string(self) -> String {
            alloc::format!("PFCP:{}:{}:{}", self.proto, self.pad_index, self.wudf_pid)
        }

        /// Parse [`to_hid_string`](Self::to_hid_string). `None` on ANY deviation — a foreign string
        /// index answered, a truncated read, a non-decimal field, trailing junk — so a host that
        /// cannot read a well-formed proof refuses to deliver instead of guessing at a pid.
        pub fn from_hid_string(s: &str) -> Option<ChannelProof> {
            let rest = s.strip_prefix("PFCP:")?;
            let mut it = rest.split(':');
            let proto = it.next()?.parse::<u32>().ok()?;
            let pad_index = it.next()?.parse::<u32>().ok()?;
            let wudf_pid = it.next()?.parse::<u32>().ok()?;
            if it.next().is_some() {
                return None; // trailing field: not a shape we minted
            }
            Some(ChannelProof {
                magic: PROOF_MAGIC,
                proto,
                pad_index,
                wudf_pid,
            })
        }
    }

    /// Bootstrap-mailbox magic (`"PFBT"` LE) — the host stamps it LAST (after `host_proto`), so a
    /// driver only trusts a fully-initialized mailbox.
    pub const BOOT_MAGIC: u32 = 0x5442_4650;

    /// `Global\pfxusb-boot-<index>` — the virtual Xbox 360 pad's bootstrap mailbox ([`PadBootstrap`]).
    pub fn xusb_boot_name(index: u8) -> String {
        alloc::format!("Global\\pfxusb-boot-{index}")
    }
    /// `Global\pfds-boot-<index>` — the DualSense / DualShock 4 pad's bootstrap mailbox
    /// ([`PadBootstrap`]).
    pub fn pad_boot_name(index: u8) -> String {
        alloc::format!("Global\\pfds-boot-{index}")
    }

    /// The per-pad bootstrap mailbox (32 B, named `Global\pf…-boot-<index>`, SY+LS DACL) — the ONLY
    /// named object left on the gamepad channel. It exists because the pad drivers are UMDF HID
    /// minidrivers with no control device (hidclass owns the stack), so there is no IOCTL to hand the
    /// driver a duplicated handle or learn its WUDFHost pid; this mailbox is the late-bound handshake:
    ///
    /// 1. host creates it (zeroed), stamps `host_proto` then `magic` (in that order);
    /// 2. driver opens it by name (pad index from `pszDeviceLocation`), writes `driver_proto`, and —
    ///    iff `host_proto` matches its own version — publishes `driver_pid`;
    /// 3. host asks the DEVNODE who the driver is ([`ChannelProof`]) — **not** the mailbox — verifies
    ///    that pid is a genuine WUDFHost, duplicates the unnamed DATA section into it, then writes
    ///    `data_handle` + `handle_pid` and bumps `handle_seq` LAST;
    /// 4. driver sees a fresh `handle_seq` addressed to its own pid, maps `data_handle`, and validates
    ///    the mapped section's magic + `pad_index` before use.
    ///
    /// **Trust boundary (v3).** This mailbox is writable by LocalService — it has to be, since that is
    /// what the driver's own WUDFHost runs as — so NOTHING in it may decide where the DATA section
    /// goes. Through v2 `driver_pid` did decide that, which was the security-review 2026-07-28 hole
    /// (see [`ChannelProof`]); step 3 now sources the pid from the device stack and `driver_pid` is
    /// advisory only — a liveness/diagnostic hint. What is left here is a handle VALUE, meaningless
    /// outside the one process it was minted for, plus pids and version numbers, none of them secret.
    /// A LocalService tamperer can therefore still deny a pad (overwrite the fields, squat the name)
    /// but can no longer read or inject: it cannot place a valid section handle in the WUDFHost, the
    /// driver's magic + `pad_index` validation rejects any handle that does not resolve to this pad's
    /// section, and the delivery target is no longer its to choose.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct PadBootstrap {
        /// [`BOOT_MAGIC`], host-stamped last at creation.
        pub magic: u32,
        /// The host's [`GAMEPAD_PROTO_VERSION`]. A driver whose own version differs must NOT publish
        /// its pid (fail closed) — it still writes `driver_proto` so the host can log the mismatch.
        pub host_proto: u32,
        /// The driver's WUDFHost process id (driver-written; `0` = no driver yet). The duplication
        /// target the host verifies (`verify_is_wudfhost`) before duplicating the DATA section into it.
        pub driver_pid: u32,
        /// The driver's [`GAMEPAD_PROTO_VERSION`] (driver-written; diagnostics only).
        pub driver_proto: u32,
        /// The DATA-section handle VALUE the host duplicated into `handle_pid`'s handle table
        /// (host-written; valid only inside that process).
        pub data_handle: u64,
        /// The pid `data_handle` was duplicated for — a driver whose pid differs ignores the delivery.
        pub handle_pid: u32,
        /// Bumped by the host (host-global monotonic, never 0) AFTER `data_handle`/`handle_pid` are in
        /// place — the driver's new-delivery trigger.
        pub handle_seq: u32,
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
        pub _pad0: [u8; 2],
        /// Written by the driver when it binds (device add) and on every serviced IOCTL:
        /// [`GAMEPAD_PROTO_VERSION`]. `0` = no driver attached — the host health check keys off it.
        pub driver_proto: u32,
        /// Bumped by the driver on every serviced XInput IOCTL — proves the game-visible path (it
        /// only advances while something polls the slot, so a static value is not an error).
        pub driver_heartbeat: u32,
        /// The pad index this section serves (host-stamped before the magic). The driver validates it
        /// against its own `pszDeviceLocation` index when it maps the delivered handle, so a mis-routed
        /// (or bootstrap-tampered) cross-pad delivery is rejected instead of silently cross-wiring two
        /// pads. Carved from v1 reserved space (v2).
        pub pad_index: u32,
        pub _reserved1: [u8; 20],
    }

    /// The legacy (pre-ring) [`PadShm`] size. Old binaries on either side were built against a
    /// 256-byte layout; every field they know sits below this offset, and the ring extension keeps
    /// bytes `0..256` byte-identical. Pagefile-backed sections are page-granular, so a view of
    /// either generation's size maps against either generation's section — but a driver must
    /// still be able to fall back to mapping this size if the full-size map is ever refused
    /// (see `pf_umdf_util::ChannelConfig::min_data_size`).
    pub const PAD_SHM_LEGACY_SIZE: usize = 256;

    /// The v2.1 output-report ring depth — the length every pre-v2.2 driver hardcodes in its
    /// `% OUT_RING_LEN` slot math, kept as the drain length whenever [`PadShm::out_ring_len`]
    /// reads 0. Its sizing assumption ("8 slots at a ~4 ms poll tolerates a sustained 2 kHz
    /// writer — double any real HID output rate") was falsified in the field: DS5 compat-vibration
    /// writers that re-send per audio quantum sustain >2 kHz for tens of seconds (field log
    /// 2026-07-30), overflowing every poll.
    pub const OUT_RING_LEN: u32 = 8;
    pub const OUT_RING_LEN_USIZE: usize = OUT_RING_LEN as usize;

    /// The v2.2 output-report ring depth — the same ring grown in place to every slot that fits
    /// the one-page section ([`PAD_SHM_SIZE`] = 4096): 56 slots at the host's ~4 ms poll
    /// tolerates a sustained 14 kHz writer, ~7× the worst rate observed in the field. Used only
    /// when BOTH sides negotiated it (host stamped `out_ring_ver >= 2`, driver echoed the length
    /// in [`PadShm::out_ring_len`]).
    pub const OUT_RING_LEN_V22: u32 = 56;
    pub const OUT_RING_LEN_V22_USIZE: usize = OUT_RING_LEN_V22 as usize;

    /// The v2.1 [`PadShm`] size. A v2.1 driver maps this much and gates its 8-slot ring writes on
    /// `mapped_len() >= 1024`; the v2.2 growth keeps bytes `0..1024` layout-identical (the ring
    /// slots 8.. extend over what was `_reserved2`).
    pub const PAD_SHM_V21_SIZE: usize = 1024;

    /// The full v2.2 [`PadShm`] size — exactly one page. This is a hard ceiling, not a choice:
    /// pagefile-backed sections round up to page granularity, which is what lets every binary
    /// generation map its own idea of the size against any other generation's section. A layout
    /// that grows past 4096 breaks that (an old section refuses the larger view) and must ride a
    /// new negotiation, not this one.
    pub const PAD_SHM_SIZE: usize = 4096;

    /// One slot of the lossless output-report ring: the report bytes as the game wrote them
    /// (report id first), with the exact length — unlike the legacy latest-report slot, whose
    /// fixed 64-byte copy can carry a stale tail from a previous longer report.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug)]
    pub struct OutSlot {
        /// Valid bytes in `data` (0..=64). `0` = never written.
        pub len: u32,
        pub data: [u8; 64],
    }

    /// Virtual DualSense / DualShock 4 shared section (4096 B — [`PAD_SHM_SIZE`]; bytes `0..256`
    /// are the v2 legacy layout verbatim — [`PAD_SHM_LEGACY_SIZE`]; bytes `0..1024` are the v2.1
    /// layout verbatim — [`PAD_SHM_V21_SIZE`]). The host writes the `0x01`-style HID input
    /// report into `input`; the driver feeds it to game `READ_REPORT`s and publishes a game's
    /// `0x02` output (rumble / lightbar / player-LEDs / adaptive triggers) twice: into the legacy
    /// latest-report `output` slot (bumping `out_seq` — every host generation reads this), and,
    /// when the host stamped `out_ring_ver`, into the lossless `out_ring` (bumping `ring_head`).
    /// The ring exists because the single slot COALESCES: a rumble-stop report overwritten by an
    /// LED/trigger report inside one host poll window was gone forever — the confirmed stuck-rumble
    /// path (`design/rumble-root-fix.md` §A). `device_type` selects the HID identity
    /// ([`DEVTYPE_DUALSENSE`] / [`DEVTYPE_DUALSHOCK4`]).
    ///
    /// Version posture: this is a TAIL extension negotiated by zeroed-reserved capability fields,
    /// deliberately NOT a [`GAMEPAD_PROTO_VERSION`] bump — the bootstrap fails CLOSED on a version
    /// mismatch (no pad at all), which is the wrong failure mode for a feedback-quality fix. An
    /// old driver never reads the new fields; an old host never stamps `out_ring_ver`, so a new
    /// driver stays on the legacy slot against it.
    ///
    /// v2.2 ring-length negotiation (the ring grown 8 → [`OUT_RING_LEN_V22`] in place): the two
    /// sides must agree on the `% len` slot math, and neither the host binary nor the driver
    /// binary can assume the other's generation, so each declares and the SHORTER understanding
    /// wins. The host stamps `out_ring_ver = 2` at section creation ("I can drain the long
    /// ring"); the driver picks its length from that stamp (`>= 2` and a full-size map → 56,
    /// `1` → 8, `0` → no ring) and ECHOES the picked length into `out_ring_len` before every
    /// `ring_head` bump. The host keys its drain off the echo alone (`0` = pre-v2.2 driver = 8),
    /// and the slot-bytes → echo → head-bump store order means a drain that Acquire-observed a
    /// head bump always reads the length that wrote those slots.
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
        pub _pad0: [u8; 3],
        /// Written by the driver's timer while it has the section mapped: [`GAMEPAD_PROTO_VERSION`].
        /// `0` = no driver attached — the host health check keys off it.
        pub driver_proto: u32,
        /// Bumped by the driver's ~125 Hz timer each tick — a true liveness heartbeat (unlike the
        /// XUSB one, this advances whenever the driver is loaded, game or not).
        pub driver_heartbeat: u32,
        /// The pad index this section serves (host-stamped before the magic) — see
        /// [`XusbShm::pad_index`]. Carved from v1 reserved space (v2).
        pub pad_index: u32,
        /// Host-stamped `1` at section creation ⇔ "this section carries the `out_ring` region and
        /// the host drains it". The section starts zeroed and an old host never writes it, so `0`
        /// tells a new driver to stay legacy-only. Carved from v2 reserved space (v2.1).
        pub out_ring_ver: u32,
        /// Driver-bumped (AFTER writing `out_ring[ring_head % len]`) count of reports ever
        /// published to the ring — the host's drain cursor compares against its own tail and
        /// detects overflow by `head - tail > len`. Same publish-then-bump store order as
        /// `out_seq` (the host's Acquire load orders the reads). Carved from v2 reserved space
        /// (v2.1).
        pub ring_head: u32,
        /// The ring length the driver's slot math is USING — the driver's side of the v2.2
        /// negotiation (see the struct docs), (re-)stamped before every `ring_head` bump. `0` =
        /// a pre-v2.2 driver that never writes it = [`OUT_RING_LEN`]. Carved from v2.1 reserved
        /// space (v2.2).
        pub out_ring_len: u32,
        /// Seqlock generation over the [`PadShm::input`] slot (host-written): **odd** while a
        /// report is mid-copy, **even** when the slot holds a whole one. The host bumps it to odd,
        /// `Release`-fences, writes the 64 bytes, then `Release`-stores it even; a driver samples
        /// it before and after its read and retries when it caught a write in flight.
        ///
        /// The input slot is a single unqueued buffer that both sides touch without a lock, so a
        /// driver read landing mid-copy hands the game a report that is half the previous frame
        /// and half the next. For buttons that is a one-tick glitch; for motion it is a spike in
        /// angular velocity, and anything integrating gyro aim turns a spike into real aim
        /// movement. (v2.3)
        ///
        /// Version posture, same as the ring's: an old driver never reads this and behaves exactly
        /// as it does today, and against an old HOST the field stays 0 — a constant even value, so
        /// a new driver's re-check always passes and it, too, behaves exactly as today. No
        /// capability stamp is needed because "never written" and "no write in flight" are the
        /// same observation. Carved from v2.2 reserved space, inside the v2 legacy region so even
        /// the smallest cross-generation map covers it.
        pub input_gen: u32,
        pub _reserved1: [u8; 84],
        /// The lossless output-report ring — [`OUT_RING_LEN`] slots under a v2.1 negotiation,
        /// [`OUT_RING_LEN_V22`] under v2.2 (slots 8.. overlay what v2.1 called `_reserved2`,
        /// which no shipped binary ever read or wrote). See the struct docs and [`OutSlot`].
        pub out_ring: [OutSlot; OUT_RING_LEN_V22_USIZE],
        pub _reserved2: [u8; 32],
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
        assert!(offset_of!(XusbShm, driver_proto) == 32);
        assert!(offset_of!(XusbShm, driver_heartbeat) == 36);
        assert!(offset_of!(XusbShm, pad_index) == 40);

        assert!(size_of::<PadShm>() == PAD_SHM_SIZE);
        assert!(offset_of!(PadShm, magic) == 0);
        assert!(offset_of!(PadShm, input) == 8);
        assert!(offset_of!(PadShm, out_seq) == 72);
        assert!(offset_of!(PadShm, output) == 76);
        assert!(offset_of!(PadShm, device_type) == 140);
        assert!(offset_of!(PadShm, driver_proto) == 144);
        assert!(offset_of!(PadShm, driver_heartbeat) == 148);
        assert!(offset_of!(PadShm, pad_index) == 152);
        // v2.1 ring extension — everything below PAD_SHM_LEGACY_SIZE is the v2 layout verbatim.
        assert!(offset_of!(PadShm, out_ring_ver) == 156);
        assert!(offset_of!(PadShm, ring_head) == 160);
        assert!(offset_of!(PadShm, out_ring) == PAD_SHM_LEGACY_SIZE);
        assert!(size_of::<OutSlot>() == 68);
        // v2.2 in-place ring growth — the echo field sits in v2.1 reserved space, the grown ring
        // stays within the v2.1 slots' historical offsets (slot k at 256 + k*68), and the whole
        // struct is exactly the one page that keeps cross-generation views mappable.
        assert!(offset_of!(PadShm, out_ring_len) == 164);
        // v2.3 input seqlock — 4-aligned (the atomic accessors check it) and inside the v2 legacy
        // region, so every driver generation's map covers it whether or not it reads it.
        assert!(offset_of!(PadShm, input_gen) == 168);
        assert!(offset_of!(PadShm, input_gen) % 4 == 0);
        assert!(offset_of!(PadShm, input_gen) < PAD_SHM_LEGACY_SIZE);
        assert!(
            PAD_SHM_LEGACY_SIZE + OUT_RING_LEN_USIZE * size_of::<OutSlot>() <= PAD_SHM_V21_SIZE
        );
        assert!(PAD_SHM_SIZE == 4096);

        assert!(size_of::<ChannelProof>() == 16);
        assert!(offset_of!(ChannelProof, magic) == 0);
        assert!(offset_of!(ChannelProof, proto) == 4);
        assert!(offset_of!(ChannelProof, pad_index) == 8);
        assert!(offset_of!(ChannelProof, wudf_pid) == 12);

        assert!(size_of::<PadBootstrap>() == 32);
        assert!(offset_of!(PadBootstrap, magic) == 0);
        assert!(offset_of!(PadBootstrap, host_proto) == 4);
        assert!(offset_of!(PadBootstrap, driver_pid) == 8);
        assert!(offset_of!(PadBootstrap, driver_proto) == 12);
        assert!(offset_of!(PadBootstrap, data_handle) == 16);
        assert!(offset_of!(PadBootstrap, handle_pid) == 24);
        assert!(offset_of!(PadBootstrap, handle_seq) == 28);
    };
}

/// Steam Controller 2 (Triton) wire tables shared by the UMDF driver (answers Steam
/// synchronously) and the host/inject side (Linux usbip leg + tests). Everything here is
/// pure byte-packing so it tests on any host.
pub mod triton {
    /// Feature-1 command bytes of the Valve query dance.
    pub const ID_GET_ATTRIBUTES_VALUES: u8 = 0x83;
    pub const ID_GET_STRING_ATTRIBUTE: u8 = 0xAE;
    pub const ID_GET_FIRMWARE_INFO: u8 = 0xF2;
    /// Output report id Steam rumbles with (`80 | type | intensity16 | Lspeed16 Lgain | Rspeed16 Rgain`).
    pub const ID_OUT_REPORT_HAPTIC_RUMBLE: u8 = 0x80;

    /// The wired Steam Controller 2 identity (`28DE:1302`) — the Triton half of
    /// [`crate::gamepad::DEVTYPE_TRITON`]'s `0x83` attributes reply.
    const WIRED_PRODUCT: u32 = 0x1302;

    /// Firmware build time (unix epoch) served as attribute tag `4`
    /// (`ATTRIB_FIRMWARE_BUILD_TIME`, per SDL's `controller_constants.h` — the same tag map the
    /// Phase-0 bench responder used) in the `0x83` attributes reply, and mirrored at bytes 4..8
    /// of the `0xF2` firmware-info reply (the two carried the same value before this constant
    /// existed and must keep doing so).
    ///
    /// BENCH FINDING (2026-08-21, the first Windows Steam claim): the previous synthetic value
    /// (`unit_id ^ 0x0296_DAF9` ≈ Feb 2016) read as decade-old firmware and Steam offered to
    /// "update" the virtual pad — a flow whose SET_REPORTs would be forwarded toward a REAL
    /// controller on the client path, which nobody wants. The synthetic attributes are this
    /// pad's permanent identity (the in-driver query dance answers Steam itself; a captured
    /// physical pad's firmware version never reaches Steam), so the value is ours to pin.
    /// `0x6A6D_3700` = 2026-08-01T00:00:00Z. Bump it when Steam learns a newer shipping
    /// firmware and starts prompting again.
    pub const FW_BUILD_TIME: u32 = 0x6A6D_3700;

    /// Bit 31 of an out-ring slot's `len` marks the frame as a FEATURE set (vs interrupt/output).
    /// Only the Triton devtype's producer (driver) and consumer (triton_windows drain) interpret
    /// it; every other devtype writes plain lengths, so the bit is additive.
    pub const OUT_FEATURE_BIT: u32 = 0x8000_0000;
    #[inline]
    pub const fn out_len(raw: u32) -> u32 {
        raw & !OUT_FEATURE_BIT
    }
    #[inline]
    pub const fn out_is_feature(raw: u32) -> bool {
        raw & OUT_FEATURE_BIT != 0
    }

    /// Wire length (id byte included) of each input report the wired descriptor declares.
    /// hidclass sizes its read buffer from the largest (0x42 → 54) and refuses over-long
    /// completions, so the driver trims every served report to this. `None` = undeclared id,
    /// drop it (0x47 is the BLE timestamp report — BLE-only, not in the 372-byte descriptor).
    pub const fn input_len(report_id: u8) -> Option<usize> {
        match report_id {
            0x42 => Some(54),
            0x45 => Some(46),
            0x43 => Some(15),
            0x44 => Some(6),
            0x79 => Some(2),
            0x7B => Some(13),
            _ => None,
        }
    }

    /// Declared wire length (id byte INCLUDED) of each OUTPUT report the wired descriptor
    /// declares. The mirror of [`input_len`] for the write direction: hidclass pads every output
    /// write to `OutputReportByteLength` (64) before the driver rings it, so the HOST trims each
    /// drained OUTPUT frame to this before forwarding — the client replays the bare declared
    /// payload over GATT, matching the Linux leg's native-length forwarding (a 0x80 rumble is
    /// 10 bytes there, not 64). An unknown id returns 64 — no trim, never guess a length.
    /// Provenance: the captured 372-byte descriptor ([`RDESC`]) plus the per-id output
    /// characteristic map, verified per-actuator against real hardware (bench 2026-06-09).
    ///
    /// ⚠ HAND-MIRRORED on the Apple client as `Sc2Device.strippedOutputLen` (id-EXCLUDED, so
    /// `stripped + 1` == the value here), which trims the GATT write. Its test transcribes these
    /// numbers, so it cannot catch a change made HERE — edit this table and you must edit that one.
    pub const fn out_report_len(id: u8) -> usize {
        match id {
            0x80 => 10,
            0x81 => 8,
            0x82 => 4,
            0x83 => 10,
            0x84 => 9,
            0x85 => 4,
            0x86 => 4,
            // 0x87/0x88/0x89 are declared full-length (63-byte payload) blobs.
            _ => 64,
        }
    }

    /// Per-pad unit id ("TRI\0" | index — same value the Linux leg uses).
    pub const fn unit_id(index: u8) -> u32 {
        0x5452_4900 | index as u32
    }

    /// ASCII serial `FVPF1302<idx:02>D03` — "FVPF" because Steam rejects a "PF"-leading
    /// serial, and the FVPF prefix is what the host's physical-conflict gate excludes.
    pub fn serial(index: u8, out: &mut [u8; 13]) {
        const D: &[u8; 10] = b"0123456789";
        out.copy_from_slice(b"FVPF130200D03");
        out[8] = D[(index / 10 % 10) as usize];
        out[9] = D[(index % 10) as usize];
    }

    /// The wired Triton's captured 372-byte report descriptor — MOVED VERBATIM from
    /// crates/pf-inject/src/inject/proto/triton_proto.rs:52-85 (`TRITON_RDESC`), including its
    /// comment block. Byte-identical to the Phase-0 sysfs capture (programmatically diffed).
    #[rustfmt::skip]
    pub static RDESC: [u8; 372] = [
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x40, 0x09, 0x01, 0xA1, 0x00,
        0x05, 0x09, 0x19, 0x01, 0x29, 0x02, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
        0x95, 0x02, 0x81, 0x02, 0x75, 0x06, 0x95, 0x01, 0x81, 0x01, 0x05, 0x01,
        0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02,
        0x81, 0x06, 0x95, 0x01, 0x09, 0x38, 0x81, 0x06, 0x05, 0x0C, 0x0A, 0x38,
        0x02, 0x95, 0x01, 0x81, 0x06, 0xC0, 0xC0, 0x05, 0x01, 0x09, 0x06, 0xA1,
        0x01, 0x85, 0x41, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x81, 0x01, 0x19, 0x00, 0x29,
        0x65, 0x15, 0x00, 0x25, 0x65, 0x75, 0x08, 0x95, 0x06, 0x81, 0x00, 0xC0,
        0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x42, 0x15, 0x00, 0x26,
        0xFF, 0x00, 0x75, 0x08, 0x95, 0x35, 0x09, 0x42, 0x81, 0x02, 0x85, 0x44,
        0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x05, 0x09, 0x44, 0x81,
        0x02, 0x85, 0x79, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x01,
        0x09, 0x79, 0x81, 0x02, 0x85, 0x43, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
        0x08, 0x95, 0x0E, 0x09, 0x43, 0x81, 0x02, 0x85, 0x7B, 0x15, 0x00, 0x26,
        0xFF, 0x00, 0x75, 0x08, 0x95, 0x0C, 0x09, 0x7B, 0x81, 0x02, 0x85, 0x45,
        0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x2D, 0x09, 0x45, 0x81,
        0x02, 0x85, 0x80, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x09,
        0x09, 0x80, 0x91, 0x02, 0x85, 0x81, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
        0x08, 0x95, 0x07, 0x09, 0x81, 0x91, 0x02, 0x85, 0x82, 0x15, 0x00, 0x26,
        0xFF, 0x00, 0x75, 0x08, 0x95, 0x03, 0x09, 0x82, 0x91, 0x02, 0x85, 0x83,
        0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x09, 0x09, 0x83, 0x91,
        0x02, 0x85, 0x84, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x08,
        0x09, 0x84, 0x91, 0x02, 0x85, 0x85, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
        0x08, 0x95, 0x03, 0x09, 0x85, 0x91, 0x02, 0x85, 0x86, 0x15, 0x00, 0x26,
        0xFF, 0x00, 0x75, 0x08, 0x95, 0x03, 0x09, 0x86, 0x91, 0x02, 0x85, 0x87,
        0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x3F, 0x09, 0x87, 0x91,
        0x02, 0x85, 0x89, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x3F,
        0x09, 0x89, 0x91, 0x02, 0x85, 0x88, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
        0x08, 0x95, 0x3F, 0x09, 0x88, 0x91, 0x02, 0x85, 0x01, 0x95, 0x3F, 0x09,
        0x01, 0xB1, 0x02, 0x85, 0x02, 0x95, 0x3F, 0x09, 0x01, 0xB1, 0x02, 0xC0,
    ];

    /// Build the reply to a feature GET_REPORT — the answer half of the Valve query dance. Steam's
    /// `GetControllerInfo` SETs a query (`0x83` attributes / `0xAE` string) and then GETs the answer;
    /// **the reply's command byte must echo the LAST SET's command** or Steam treats the pad as
    /// broken and never adopts it (confirmed on-glass 2026-07-15: answering every GET with a serial
    /// blob left the virtual pad unpicked). The frame rides feature report id **1**
    /// (`[0x01][cmd][len][payload…]`, matching SDL's send framing for this device), and the `0x83`
    /// blob carries the Triton's product id. The attribute VALUES beyond the product id mirror the
    /// Deck's hidraw capture (same firmware family conventions) — replace them with a capture from
    /// a physical pad if Steam still balks.
    ///
    /// `last_set` is the id-first SET payload (`[0x01, cmd, …]`); a stack that already stripped the
    /// id byte (`[cmd, …]`, cmd ≥ 0x80) is handled too.
    ///
    /// MOVED VERBATIM from `crates/pf-inject/src/inject/proto/triton_proto.rs:290-365`
    /// (fn `triton_feature_reply`) — do not re-derive it. Two mechanical fixups the move needed:
    ///   (1) the body referenced the module const `TRITON_WIRED_PRODUCT` (triton_proto.rs:31) —
    ///       repointed to this module's [`WIRED_PRODUCT`];
    ///   (2) `ATTRIB_STR_UNIT_SERIAL` and the `ID_*` consts were declared INSIDE the fn body —
    ///       the `ID_*` consts now come from this module (shadow-free); `ATTRIB_STR_UNIT_SERIAL`
    ///       stays fn-local, since it isn't part of this module's public surface.
    pub fn feature_reply(last_set: &[u8], serial: &str, unit_id: u32) -> [u8; 64] {
        const ATTRIB_STR_UNIT_SERIAL: u8 = 0x01;

        let body = match last_set {
            [0x01, rest @ ..] => rest,
            d => d,
        };
        let cmd = body.first().copied().unwrap_or(ID_GET_STRING_ATTRIBUTE);

        let mut r = [0u8; 64];
        r[0] = 0x01;
        match cmd {
            ID_GET_ATTRIBUTES_VALUES => {
                // Captured controller response: 25-byte payload containing five id/u32 attributes.
                r[1] = ID_GET_ATTRIBUTES_VALUES;
                r[2] = 0x19;
                let attrs = [
                    (0x01, WIRED_PRODUCT),
                    (0x02, 0),
                    (0x0A, unit_id),
                    // Tag 4 = ATTRIB_FIRMWARE_BUILD_TIME. Was `unit_id ^ 0x0296_DAF9` (≈ Feb
                    // 2016 as an epoch) — Steam read it as ancient firmware and prompted to
                    // update the virtual pad. See [`FW_BUILD_TIME`].
                    (0x04, FW_BUILD_TIME),
                    (0x09, 0x49),
                ];
                let mut o = 3;
                for (id, val) in attrs {
                    r[o] = id;
                    r[o + 1..o + 5].copy_from_slice(&val.to_le_bytes());
                    o += 5;
                }
            }
            ID_GET_STRING_ATTRIBUTE => {
                // Captured replies always declare 20 bytes: attribute id plus a 19-byte padded string.
                let attr = body.get(2).copied().unwrap_or(ATTRIB_STR_UNIT_SERIAL);
                let b = serial.as_bytes();
                let len = b.len().min(19);
                r[..4].copy_from_slice(&[0x01, ID_GET_STRING_ATTRIBUTE, 0x14, attr]);
                r[4..4 + len].copy_from_slice(&b[..len]);
            }
            ID_GET_FIRMWARE_INFO => {
                let index = body.get(2).copied().unwrap_or(0);
                r[1] = ID_GET_FIRMWARE_INFO;
                r[3] = index;
                match index {
                    0 => {
                        r[2] = 0x29;
                        // Mirrors the 0x83 reply's tag-4 attribute — the two build-time fields
                        // carried the same value before [`FW_BUILD_TIME`] existed and must keep
                        // agreeing (Steam may cross-check them).
                        r[4..8].copy_from_slice(&FW_BUILD_TIME.to_le_bytes());
                        r[8] = 0x49;
                        r[12..24].copy_from_slice(b"603f69218a85");
                        let b = serial.as_bytes();
                        let len = b.len().min(16);
                        r[28..28 + len].copy_from_slice(&b[..len]);
                    }
                    1 => {
                        r[2] = 0x22;
                        r[4..37].copy_from_slice(&[
                            0x00, 0x57, 0xD0, 0x18, 0x6A, 0x37, 0x30, 0x35, 0x34, 0x32, 0x35, 0x37,
                            0x64, 0x32, 0x64, 0x61, 0x37, 0x00, 0x00, 0x00, 0x00, 0x23, 0x00, 0x00,
                            0x00, 0x00, 0x00, 0x00, 0x00, 0x33, 0x6D, 0x02, 0x00,
                        ]);
                    }
                    _ => {
                        r[2] = 0x09;
                        r[4..12].copy_from_slice(&[0x7C, 0x4F, 0x01, 0x00, 0x01, 0, 0, 0]);
                    }
                }
            }
            _ => {
                let n = body.len().min(63);
                r[1..1 + n].copy_from_slice(&body[..n]);
            }
        }
        r
    }
}

/// Virtual-pointer shared-memory layout (host ↔ the UMDF HID-mouse minidriver `pf_mouse`).
///
/// Why a virtual mouse exists at all: with no pointing device present (a headless Windows host —
/// no dongle attached), win32k reports the cursor as absent (`SM_MOUSEPRESENT` = 0) and DWM never
/// composites a cursor into the pf-vdisplay frame, so a streamed desktop has an invisible pointer
/// even though `SendInput` moves it. A resident HID mouse devnode makes Windows always consider a
/// pointer present — the Sunshine/Parsec-class fix. Injection stays `SendInput`; the report path
/// below exists for validation (`vmouse-spike`) and as the future higher-fidelity route.
///
/// The channel is the **sealed pad channel** verbatim (`design/gamepad-channel-sealing.md`): the
/// same [`gamepad::PadBootstrap`] mailbox handshake (and therefore the same
/// [`gamepad::GAMEPAD_PROTO_VERSION`] lockstep), a mouse-specific mailbox name
/// ([`mouse_boot_name`]) and DATA magic, and `pad_index` validation (a single resident mouse =
/// index 0). Reusing the handshake means `pf-umdf-util`'s audited `ChannelClient`/`PadChannel`
/// serve the mouse unchanged.
pub mod mouse {
    use alloc::string::String;
    use bytemuck::{Pod, Zeroable};

    /// Mouse DATA-section magic ("PFMO" LE) — distinct from the pad magics so a cross-wired
    /// delivery fails validation.
    pub const MOUSE_MAGIC: u32 = 0x4F4D_4650;

    /// `Global\pfmouse-boot-<index>` — the virtual mouse's bootstrap mailbox
    /// ([`crate::gamepad::PadBootstrap`]).
    pub fn mouse_boot_name(index: u8) -> String {
        alloc::format!("Global\\pfmouse-boot-{index}")
    }

    /// HID identity both sides report/expect ("PF" / "MO" — an obviously-virtual identity; no
    /// software matches on it, unlike the pads' cloned Sony/Valve ids).
    pub const MOUSE_VID: u16 = 0x5046;
    pub const MOUSE_PID: u16 = 0x4D4F;
    pub const MOUSE_VER: u16 = 0x0100;

    /// The one input report (id `0x01`): `[id, buttons(5 bits), x_lo, x_hi, y_lo, y_hi, wheel,
    /// pan]` — absolute X/Y over `0..=`[`MOUSE_ABS_MAX`], relative wheel/pan.
    pub const MOUSE_REPORT_ID: u8 = 0x01;
    pub const MOUSE_REPORT_LEN: usize = 8;
    /// Logical maximum of the absolute X/Y axes (15-bit, the HID-descriptor convention).
    pub const MOUSE_ABS_MAX: u16 = 0x7FFF;

    /// Build the 8-byte input report. Pure so the byte layout is unit-tested on every dev machine
    /// (the driver workspace is `panic = "abort"` and hosts no test harness); the driver only
    /// ferries these bytes, it never builds them.
    #[must_use]
    pub fn input_report(buttons: u8, x: u16, y: u16, wheel: i8, pan: i8) -> [u8; MOUSE_REPORT_LEN] {
        let x = x.min(MOUSE_ABS_MAX);
        let y = y.min(MOUSE_ABS_MAX);
        [
            MOUSE_REPORT_ID,
            buttons & 0x1F,
            (x & 0xFF) as u8,
            (x >> 8) as u8,
            (y & 0xFF) as u8,
            (y >> 8) as u8,
            wheel as u8,
            pan as u8,
        ]
    }

    /// Virtual-mouse shared section (64 B). The host writes an input report then bumps `in_seq`
    /// (Release); the driver's timer Acquire-loads `in_seq` and completes a pended `READ_REPORT`
    /// with the fresh report — event-driven like a real mouse, so an idle section generates NO
    /// HID traffic (a constant report stream would read as user activity to the OS).
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug)]
    pub struct MouseShm {
        pub magic: u32,
        /// Bumped by the host AFTER `report` is in place (Release) — the driver's new-input
        /// trigger. `0` = nothing published yet.
        pub in_seq: u32,
        /// The latest HID input report (id [`MOUSE_REPORT_ID`], [`MOUSE_REPORT_LEN`] bytes).
        pub report: [u8; MOUSE_REPORT_LEN],
        /// Written by the driver's timer while attached: [`crate::gamepad::GAMEPAD_PROTO_VERSION`]
        /// (the mouse channel rides the gamepad handshake). `0` = no driver attached — the host
        /// health check keys off it.
        pub driver_proto: u32,
        /// Bumped by the driver's timer each tick — liveness (advances whether or not input flows).
        pub driver_heartbeat: u32,
        /// The device index this section serves (host-stamped before the magic; the driver
        /// validates it against its devnode Location — same fail-closed check as the pads).
        pub pad_index: u32,
        pub _reserved: [u8; 36],
    }

    // Offsets are the cross-process wire contract — pin every one (same discipline as `gamepad`).
    const _: () = {
        use core::mem::{offset_of, size_of};

        assert!(size_of::<MouseShm>() == 64);
        assert!(offset_of!(MouseShm, magic) == 0);
        assert!(offset_of!(MouseShm, in_seq) == 4);
        assert!(offset_of!(MouseShm, report) == 8);
        assert!(offset_of!(MouseShm, driver_proto) == 16);
        assert!(offset_of!(MouseShm, driver_heartbeat) == 20);
        assert!(offset_of!(MouseShm, pad_index) == 24);
    };
}

/// The v5 hardware-cursor channel (remote-desktop-sweep M2c): one unnamed file mapping per
/// monitor, host-created, delivered by handle value ([`control::IOCTL_SET_CURSOR_CHANNEL`]).
/// The DRIVER's cursor thread (woken by its IddCx `hNewCursorDataAvailable` event) seqlock-writes
/// shape + position + visibility; the HOST reads at its encode-tick pace — no event crosses the
/// boundary. Writer: bump [`CursorShm::seq`] to ODD, write fields (+ shape bytes when the OS said
/// the shape changed), bump to EVEN. Reader: read seq (retry while odd), copy, re-read seq —
/// unchanged ⇒ consistent snapshot. Position-only updates never touch the shape bytes, so a
/// reader that skips unchanged `shape_id`s never copies torn pixels.
pub mod cursor {
    use bytemuck::{Pod, Zeroable};

    /// First field of [`CursorShm`] — `b"PFCU"` little-endian; anything else = not attached yet.
    pub const CURSOR_MAGIC: u32 = u32::from_le_bytes(*b"PFCU");

    /// Max cursor side (px) the driver declares to the OS (`IDDCX_CURSOR_CAPS::MaxX/MaxY`) and
    /// the section's shape buffer is sized for. Windows XL accessibility cursors top out here;
    /// the host's wire forwarder downscales to its own cap anyway.
    pub const CURSOR_SHAPE_MAX: u32 = 256;

    /// Shape-buffer bytes: 32-bpp at the declared max.
    pub const CURSOR_SHAPE_BYTES: usize = (CURSOR_SHAPE_MAX * CURSOR_SHAPE_MAX * 4) as usize;

    /// Byte offset of the shape pixels inside the section (one cache-line-ish header).
    pub const CURSOR_SHAPE_OFFSET: usize = 64;

    /// Total section size.
    pub const CURSOR_SHM_SIZE: usize = CURSOR_SHAPE_OFFSET + CURSOR_SHAPE_BYTES;

    /// `IDDCX_CURSOR_SHAPE_TYPE` values mirrored for the host (the driver writes the OS value
    /// verbatim into [`CursorShm::cursor_type`]).
    pub const CURSOR_TYPE_MASKED_COLOR: u32 = 1;
    pub const CURSOR_TYPE_ALPHA: u32 = 2;

    /// The section header (the shape pixels follow at [`CURSOR_SHAPE_OFFSET`]). `x`/`y` are the
    /// shape's TOP-LEFT in desktop coordinates (the IddCx `IDARG_OUT_QUERY_HWCURSOR::X/Y`
    /// convention — position − hotspot, can be negative); `shape_id` is the OS's per-set counter
    /// (bumps on every shape set, the overlay serial); pixels are the OS's 32-bpp rows at
    /// `pitch` bytes (BGRA for ALPHA; color+mask for MASKED_COLOR — the host converts).
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct CursorShm {
        pub magic: u32,
        /// Seqlock: odd = writer mid-update.
        pub seq: u32,
        pub visible: u32,
        pub cursor_type: u32,
        pub x: i32,
        pub y: i32,
        pub shape_id: u32,
        pub width: u32,
        pub height: u32,
        pub pitch: u32,
        pub hot_x: u32,
        pub hot_y: u32,
        /// Reserved expansion room up to [`CURSOR_SHAPE_OFFSET`].
        pub _reserved: [u32; 4],
    }

    // Layout is load-bearing across the process boundary — pin it.
    const _: () = {
        use core::mem::{offset_of, size_of};
        assert!(size_of::<CursorShm>() == 64);
        assert!(size_of::<CursorShm>() <= CURSOR_SHAPE_OFFSET);
        assert!(offset_of!(CursorShm, magic) == 0);
        assert!(offset_of!(CursorShm, seq) == 4);
        assert!(offset_of!(CursorShm, visible) == 8);
        assert!(offset_of!(CursorShm, cursor_type) == 12);
        assert!(offset_of!(CursorShm, x) == 16);
        assert!(offset_of!(CursorShm, y) == 20);
        assert!(offset_of!(CursorShm, shape_id) == 24);
        assert!(offset_of!(CursorShm, width) == 28);
        assert!(offset_of!(CursorShm, height) == 32);
        assert!(offset_of!(CursorShm, pitch) == 36);
        assert!(offset_of!(CursorShm, hot_x) == 40);
        assert!(offset_of!(CursorShm, hot_y) == 44);
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    #[test]
    fn dtd_encodes_the_session_mode() {
        // 1920×1080@60 with the fixed RB blanking: totals 2000×1125 → 135.00 MHz = 13500 ×10 kHz.
        let d = edid::dtd(1920, 1080, 60).expect("1080p60 fits the DTD encoding");
        assert_eq!(u16::from_le_bytes([d[0], d[1]]), 13_500);
        // Active dimensions round-trip through the split 8+4-bit fields.
        assert_eq!(u32::from(d[2]) | (u32::from(d[4] >> 4) << 8), 1920);
        assert_eq!(u32::from(d[5]) | (u32::from(d[7] >> 4) << 8), 1080);
        // Blanking fields carry the fixed geometry; flags match the legacy descriptor.
        assert_eq!(u32::from(d[3]) | (u32::from(d[4] & 0x0F) << 8), 80);
        assert_eq!(u32::from(d[6]) | (u32::from(d[7] & 0x0F) << 8), 45);
        assert_eq!(d[17], 0x1E);
    }

    #[test]
    fn dtd_rejects_what_the_encoding_cannot_carry() {
        // 4K120: (3840+80)·(2160+45)·120 ≈ 1.037 GHz — past the u16 10 kHz pixel-clock field.
        assert_eq!(edid::dtd(3840, 2160, 120), None);
        // 4K60 fits (≈518 MHz).
        assert!(edid::dtd(3840, 2160, 60).is_some());
        // Degenerate and over-wide modes are refused, not mis-encoded.
        assert_eq!(edid::dtd(0, 1080, 60), None);
        assert_eq!(edid::dtd(5000, 1080, 10), None);
    }

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
    fn opened_detail_roundtrips_and_saturates() {
        use frame::{pack_opened_detail, unpack_opened_detail, OPENED_DETAIL_LIVE};
        // Zero counters still stamp LIVE — "attached, nothing offered yet" is information.
        assert_eq!(pack_opened_detail(0, 0), OPENED_DETAIL_LIVE);
        assert_eq!(unpack_opened_detail(pack_opened_detail(0, 0)), Some((0, 0)));
        // Roundtrip within range.
        assert_eq!(
            unpack_opened_detail(pack_opened_detail(1234, 567)),
            Some((1234, 567))
        );
        // Saturation at each counter's width (15-bit offered, 16-bit mismatched).
        assert_eq!(
            unpack_opened_detail(pack_opened_detail(u32::MAX, u32::MAX)),
            Some((0x7FFF, 0xFFFF))
        );
        // A pre-detail driver's field (any value without the LIVE bit) carries no information.
        assert_eq!(unpack_opened_detail(0), None);
        assert_eq!(unpack_opened_detail(0x7FFF_FFFF), None);
    }

    /// The v3 tail is reachable only when BOTH the stamped layout and the declared section say so
    /// — every old/new pairing from the plan's compatibility contract, pinned.
    #[test]
    fn v3_tail_gate_covers_every_pairing() {
        use frame::{v3_readable, HEADER_V2_SIZE, HEADER_V3_SIZE};
        let (v2, v3) = (HEADER_V2_SIZE as u32, HEADER_V3_SIZE as u32);
        // new host + new driver: the only pairing that opens the tail.
        assert!(v3_readable(3, v3));
        // old host (v2 header, request `_pad` = 0) + new driver: prefix only.
        assert!(!v3_readable(2, 0));
        // new host that stamped v3 but a short section (a bug, or a truncated mapping): refused.
        assert!(!v3_readable(3, v2));
        assert!(!v3_readable(3, v3 - 1));
        // a section large enough but a v2 layout stamp: refused — the stamp is the contract.
        assert!(!v3_readable(2, v3));
        // a future host: still readable as v3 (additive versions).
        assert!(v3_readable(4, v3 + 64));
    }

    /// Capabilities activate only where both sides agree; an unknown bit on one side is inert.
    #[test]
    fn capabilities_negotiate_by_intersection() {
        use frame::*;
        let host = CAP_RING_HEALTH_V3 | CAP_FENCE_RING | CAP_SOURCE_SEQUENCE_QPC;
        let old_driver = 0;
        assert_eq!(
            negotiate(host, old_driver),
            0,
            "a pre-v3 driver advertises nothing"
        );
        let driver = CAP_RING_HEALTH_V3 | CAP_SOURCE_SEQUENCE_QPC | CAP_SWAPCHAIN_RESET;
        let n = negotiate(host, driver);
        assert_eq!(n, CAP_RING_HEALTH_V3 | CAP_SOURCE_SEQUENCE_QPC);
        assert_eq!(n & CAP_FENCE_RING, 0, "fence transport needs BOTH sides");
        assert_eq!(
            n & CAP_SWAPCHAIN_RESET,
            0,
            "an actuator the host did not ask for stays off"
        );
    }

    /// State words decode exactly; anything unrecognised is Dead, never Active by accident.
    #[test]
    fn health_state_decodes_conservatively() {
        use frame::HealthState;
        assert_eq!(HealthState::from_u32(0), HealthState::Initializing);
        assert_eq!(HealthState::from_u32(1), HealthState::Active);
        assert_eq!(HealthState::from_u32(2), HealthState::Rebuilding);
        assert_eq!(HealthState::from_u32(3), HealthState::Dead);
        assert_eq!(HealthState::from_u32(0xdead_beef), HealthState::Dead);
        assert!(frame::snapshot_consistent(1, 1));
        assert!(
            !frame::snapshot_consistent(1, 2),
            "a state flip mid-read tears the snapshot"
        );
    }

    #[test]
    fn shared_header_is_pod_and_152_bytes() {
        let mut h = frame::SharedHeader::zeroed();
        h.magic = frame::MAGIC;
        h.width = 5120;
        h.height = 1440;
        h.target_id = 262;
        h.drain_heartbeat_qpc = 0x1234_5678_9abc_def0;
        h.source_sequence = 77;
        let bytes = bytemuck::bytes_of(&h);
        assert_eq!(bytes.len(), frame::HEADER_V3_SIZE);
        // The v2 prefix is byte-identical: a v2 driver mapping the first 88 bytes sees its layout.
        assert_eq!(
            bytes[80..88],
            0u64.to_le_bytes(),
            "offered_total sits at the v2 tail end"
        );
        assert_eq!(
            bytes[120..128],
            77u64.to_le_bytes(),
            "source_sequence is v3-only, at 120"
        );
        let back: frame::SharedHeader = *bytemuck::from_bytes(bytes);
        assert_eq!(back.magic, frame::MAGIC);
        assert_eq!(back.width, 5120);
        assert_eq!(back.height, 1440);
        // v3: the monitor binding occupies the old `_pad` slot at offset 28 — byte-compatible (a v2
        // host left it zero there).
        assert_eq!(bytes[28..32], 262u32.to_le_bytes());
        // Header v2: the telemetry tail is appended — the first 64 bytes ARE the v1 layout, so a
        // pre-telemetry driver reading its 64-byte view sees exactly what it always saw.
        assert_eq!(bytes[64..72], 0x1234_5678_9abc_def0u64.to_le_bytes());
        assert_eq!(back.last_acquire_qpc, 0);
        assert_eq!(back.offered_total, 0);
    }

    #[test]
    fn attach_check_binds_ring_to_monitor() {
        use frame::{check_attach, AttachReject, MAGIC};
        // The good path: magic + matching generation + matching monitor binding.
        assert_eq!(check_attach(MAGIC, 7, 262, 7, 262), Ok(()));
        // Missing magic / superseded generation → Stale (silent drop, re-delivery coming) — and
        // staleness WINS over a binding mismatch, since a superseded delivery's binding is
        // meaningless (the fresh one re-validates).
        assert_eq!(
            check_attach(0, 7, 262, 7, 262),
            Err(AttachReject::Stale),
            "no magic"
        );
        assert_eq!(
            check_attach(MAGIC, 8, 262, 7, 262),
            Err(AttachReject::Stale),
            "recreated ring"
        );
        assert_eq!(
            check_attach(0, 8, 999, 7, 262),
            Err(AttachReject::Stale),
            "stale outranks bind"
        );
        // The v3 hardening: a fresh, magic-valid ring naming a DIFFERENT monitor fails closed.
        assert_eq!(
            check_attach(MAGIC, 7, 999, 7, 262),
            Err(AttachReject::BindMismatch)
        );
        // A v2-host header (never stamped, target_id = 0) also fails closed against a v3 driver —
        // the GET_INFO handshake rejects that pairing first, but the channel must not rely on it.
        assert_eq!(
            check_attach(MAGIC, 7, 0, 7, 262),
            Err(AttachReject::BindMismatch)
        );
    }

    #[test]
    fn control_structs_roundtrip_through_bytes() {
        let req = control::AddRequest {
            session_id: 0xDEAD_BEEF_CAFE_F00D,
            width: 3840,
            height: 2160,
            refresh_hz: 120,
            preferred_monitor_id: 7,
            max_luminance_nits: 800,
            max_frame_avg_nits: 400,
            min_luminance_millinits: 50, // 0.05 nits
            hw_cursor: 1,
        };
        let bytes = bytemuck::bytes_of(&req);
        assert_eq!(bytes.len(), 40);
        assert_eq!(*bytemuck::from_bytes::<control::AddRequest>(bytes), req);
        // preferred_monitor_id occupies the old `_reserved` slot at offset 20 — byte-compatible.
        assert_eq!(bytes[20..24], 7u32.to_le_bytes());
        // The client-HDR luminance tail rides after the legacy boundary; a zero-filled tail decodes
        // as "unknown" (the un-upgraded-host form the driver's legacy read synthesizes).
        assert_eq!(bytes[24..28], 800u32.to_le_bytes());
        let mut legacy = [0u8; 40];
        legacy[..control::ADD_REQUEST_LEGACY_SIZE]
            .copy_from_slice(&bytes[..control::ADD_REQUEST_LEGACY_SIZE]);
        // `pod_read_unaligned`, NOT `from_bytes` — same rule as `ChannelProof::parse` above, and
        // for the same reason. `legacy` is a `[u8; 40]` (align 1) but `AddRequest` opens with a
        // `u64`, so it is align 8; `from_bytes` takes a REFERENCE into the buffer and panics
        // unless the buffer happens to be 8-aligned. A stack `[u8; 40]` usually is, which is why
        // this passed everywhere for so long — Miri caught it because Miri does not let an
        // accidentally-favourable stack slot stand in for a guarantee.
        let old = bytemuck::pod_read_unaligned::<control::AddRequest>(&legacy);
        assert_eq!(old.preferred_monitor_id, 7);
        assert_eq!(
            (
                old.max_luminance_nits,
                old.max_frame_avg_nits,
                old.min_luminance_millinits
            ),
            (0, 0, 0)
        );

        let reply = control::AddReply {
            adapter_luid_low: 0x1234_5678,
            adapter_luid_high: -2,
            target_id: 262,
            resolved_monitor_id: 7,
            wudf_pid: 4242,
            cursor_excluded: 1,
        };
        let rbytes = bytemuck::bytes_of(&reply);
        assert_eq!(rbytes.len(), 24);
        assert_eq!(*bytemuck::from_bytes::<control::AddReply>(rbytes), reply);
        // resolved_monitor_id occupies the old `_reserved` slot at offset 12 — byte-compatible.
        assert_eq!(rbytes[12..16], 7u32.to_le_bytes());
        // The v2 duplication-target pid trails at offset 16.
        assert_eq!(rbytes[16..20], 4242u32.to_le_bytes());
        // The cursor-excluded tail rides after the legacy boundary; an un-upgraded driver writes
        // only the prefix, so a zero-filled tail reads as "unknown/clean" (see the field docs).
        assert_eq!(rbytes[20..24], 1u32.to_le_bytes());
        assert_eq!(control::ADD_REPLY_LEGACY_SIZE, 20);
    }

    #[test]
    fn frame_channel_request_roundtrips_through_bytes() {
        let mut req = control::SetFrameChannelRequest {
            target_id: 262,
            generation: 3,
            ring_len: frame::RING_LEN,
            header_bytes: frame::HEADER_V3_SIZE as u32,
            header_handle: 0x0000_0000_0000_1a2c,
            event_handle: 0x0000_0000_0000_1b30,
            texture_handles: [0; control::RING_LEN_USIZE],
        };
        for (k, t) in req.texture_handles.iter_mut().enumerate() {
            *t = 0x2000 + k as u64 * 4;
        }
        let bytes = bytemuck::bytes_of(&req);
        assert_eq!(bytes.len(), 32 + 8 * control::RING_LEN_USIZE);
        assert_eq!(
            *bytemuck::from_bytes::<control::SetFrameChannelRequest>(bytes),
            req
        );
        // The handle values ride at 8-byte alignment from offset 16 (header, event, then the ring).
        assert_eq!(bytes[16..24], 0x1a2cu64.to_le_bytes());
        assert_eq!(bytes[24..32], 0x1b30u64.to_le_bytes());
        assert_eq!(bytes[32..40], 0x2000u64.to_le_bytes());
    }

    #[test]
    fn update_modes_request_roundtrips_and_versions_cohere() {
        let req = control::UpdateModesRequest {
            session_id: 42,
            width: 2560,
            height: 1409, // deliberately arbitrary — the in-place path serves window-drag modes
            refresh_hz: 120,
            _reserved: 0,
        };
        let bytes = bytemuck::bytes_of(&req);
        assert_eq!(bytes.len(), 24);
        assert_eq!(
            *bytemuck::from_bytes::<control::UpdateModesRequest>(bytes),
            req
        );
        assert_eq!(bytes[8..12], 2560u32.to_le_bytes());
        // The compat window: v4–v6 are additive over v3, so the host floor stays at 3.
        assert_eq!(PROTOCOL_VERSION, 6);
        assert_eq!(MIN_DRIVER_PROTOCOL_VERSION, 3);
    }

    #[test]
    fn cursor_shm_layout_is_pinned() {
        use cursor::*;
        // The header must leave the shape offset intact whatever grows inside `_reserved`.
        assert_eq!(core::mem::size_of::<CursorShm>(), 64);
        assert_eq!(CURSOR_SHM_SIZE, 64 + 256 * 256 * 4);
        assert_eq!(CURSOR_MAGIC, u32::from_le_bytes(*b"PFCU"));
        // Seqlock snapshot discipline survives a bytemuck roundtrip.
        let hdr = CursorShm {
            magic: CURSOR_MAGIC,
            seq: 2,
            visible: 1,
            cursor_type: CURSOR_TYPE_ALPHA,
            x: -3,
            y: 7,
            shape_id: 42,
            width: 32,
            height: 32,
            pitch: 128,
            hot_x: 4,
            hot_y: 5,
            _reserved: [0; 4],
        };
        let bytes = bytemuck::bytes_of(&hdr);
        assert_eq!(*bytemuck::from_bytes::<CursorShm>(bytes), hdr);
        assert_eq!(bytes[16..20], (-3i32).to_le_bytes());
    }

    #[test]
    fn gamepad_names_and_magics_are_stable() {
        assert_eq!(gamepad::xusb_boot_name(0), "Global\\pfxusb-boot-0");
        assert_eq!(gamepad::pad_boot_name(2), "Global\\pfds-boot-2");
        // Lock the exact u32 magics the shipped host/drivers use (inject/{gamepad,dualsense}_windows.rs).
        assert_eq!(gamepad::XUSB_MAGIC, 0x5558_4650);
        assert_eq!(gamepad::PAD_MAGIC, 0x5046_4453);
        // "PFBT" little-endian.
        assert_eq!(gamepad::BOOT_MAGIC.to_le_bytes(), *b"PFBT");
    }

    #[test]
    fn pad_bootstrap_roundtrips_through_bytes() {
        let b = gamepad::PadBootstrap {
            magic: gamepad::BOOT_MAGIC,
            host_proto: gamepad::GAMEPAD_PROTO_VERSION,
            driver_pid: 1234,
            driver_proto: gamepad::GAMEPAD_PROTO_VERSION,
            data_handle: 0x0000_0000_0000_2a4c,
            handle_pid: 1234,
            handle_seq: 7,
        };
        let bytes = bytemuck::bytes_of(&b);
        assert_eq!(bytes.len(), 32);
        assert_eq!(*bytemuck::from_bytes::<gamepad::PadBootstrap>(bytes), b);
        // The handle value rides 8-aligned at offset 16; the seq trails at 28 (written LAST by the host).
        assert_eq!(bytes[16..24], 0x2a4cu64.to_le_bytes());
        assert_eq!(bytes[28..32], 7u32.to_le_bytes());
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
            control::IOCTL_SET_FRAME_CHANNEL,
            control::IOCTL_UPDATE_MODES,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn cta_luminance_codes_hit_the_reference_points() {
        // The driver's historical built-in EDID block: 0x8A ≈ 993 nits, 0x60 = 400 nits (exact),
        // 0x12 for a ~0.05-nit floor. Our coder must land on the same bytes for those volumes.
        assert_eq!(edid::cta_max_millinits(0x60), 400_000); // 50·2^3 exactly
        assert_eq!(edid::cta_max_millinits(0x8A) / 1000, 993);
        assert_eq!(edid::cta_max_luminance_code(400), 0x60);
        // 0x8A decodes to 993.481 nits, so 994 is the smallest whole-nit input that reaches it
        // under the never-advertise-brighter floor.
        assert_eq!(edid::cta_max_luminance_code(994), 0x8A);
        assert_eq!(edid::cta_min_luminance_code(50, 0x8A), 0x12); // 0.05 nits @ a 993-nit max
                                                                  // Floor semantics: never advertise brighter than the panel. 1000 nits sits between
                                                                  // code 138 (993) and 139 (~1015) → 138.
        assert_eq!(edid::cta_max_luminance_code(1000), 138);
        assert!(edid::cta_max_millinits(edid::cta_max_luminance_code(1000)) <= 1_000_000);
        // Every real code decodes below or at its input (round-down), within one step (~2.2%).
        // (Starts above code 1's 51.094 nits — beneath that the documented clamp-to-1 wins.)
        for nits in [52u32, 80, 120, 250, 400, 604, 800, 1_499, 4_000, 10_000] {
            let c = edid::cta_max_luminance_code(nits);
            let dec = edid::cta_max_millinits(c);
            assert!(dec <= nits as u64 * 1000, "{nits} → {c} decoded {dec}");
            assert!(
                dec * 1023 / 1000 >= nits as u64 * 1000,
                "{nits} → {c} more than a step low"
            );
        }
        // Clamps: 0/tiny stays a valid on-wire code (callers gate presence on nits > 0); the
        // ceiling saturates at 255.
        assert_eq!(edid::cta_max_luminance_code(0), 1);
        assert_eq!(edid::cta_max_luminance_code(u32::MAX), 255);
        // Min-luminance: 0 = unknown/true black stays 0; a floor brighter than the max clamps.
        assert_eq!(edid::cta_min_luminance_code(0, 0x8A), 0);
        assert_eq!(edid::cta_min_luminance_code(u32::MAX, 1), 255);
        // Round-trip a typical HDR400 panel: max 400 nits / min 0.4 nits.
        let max_c = edid::cta_max_luminance_code(400);
        let min_c = edid::cta_min_luminance_code(400, max_c);
        // decode: L_min = L_max·(cv/255)²/100 — must come back within ~10% of 0.4 nits.
        let back =
            edid::cta_max_millinits(max_c) * (min_c as u64 * min_c as u64) / (255 * 255) / 100;
        assert!((360..=440).contains(&back), "min decoded {back} millinits");
    }

    #[test]
    fn mouse_report_and_names_are_stable() {
        assert_eq!(mouse::mouse_boot_name(0), "Global\\pfmouse-boot-0");
        // "PFMO" little-endian, and never colliding with a pad magic (cross-wire validation).
        assert_eq!(mouse::MOUSE_MAGIC.to_le_bytes(), *b"PFMO");
        assert_ne!(mouse::MOUSE_MAGIC, gamepad::XUSB_MAGIC);
        assert_ne!(mouse::MOUSE_MAGIC, gamepad::PAD_MAGIC);
        // The 8-byte report layout the driver ferries and the host builds.
        let r = mouse::input_report(0b0000_0101, 0x1234, 0x7FFF, -3, 7);
        assert_eq!(r, [0x01, 0x05, 0x34, 0x12, 0xFF, 0x7F, 0xFD, 0x07]);
        // Clamps: axes to the 15-bit logical max, buttons to the declared 5.
        let r = mouse::input_report(0xFF, 0xFFFF, 0, 0, 0);
        assert_eq!((r[1], r[2], r[3]), (0x1F, 0xFF, 0x7F));
        // A zeroed section reads as "nothing published" (in_seq 0) — the driver's idle state.
        let shm = mouse::MouseShm::zeroed();
        assert_eq!(shm.in_seq, 0);
        assert_eq!(bytemuck::bytes_of(&shm).len(), 64);
    }

    #[test]
    fn guid_is_not_sudovda() {
        const SUDOVDA: u128 = 0xE5BC_C234_1E0C_418A_A0D4_EF8B_7501_414D;
        assert_ne!(PF_VDISPLAY_INTERFACE_GUID_U128, SUDOVDA);
    }

    /// The channel proof is what the host trusts INSTEAD of the mailbox's `driver_pid`
    /// (security-review 2026-07-28), so both wire forms — the `pf_xusb` IOCTL struct and the HID
    /// indexed-string text the two minidrivers answer — have to survive a round trip byte-for-byte,
    /// and every malformed shape has to be rejected rather than half-parsed into a pid the host
    /// would then duplicate a live input section into.
    #[test]
    fn channel_proof_round_trips_in_both_wire_forms() {
        use gamepad::*;
        let proof = ChannelProof::new(2, 4242);
        assert_eq!(proof.magic, PROOF_MAGIC);
        assert_eq!(PROOF_MAGIC, u32::from_le_bytes(*b"PFCP"));
        assert_eq!(proof.proto, GAMEPAD_PROTO_VERSION);

        // XUSB IOCTL form: the raw 16-byte struct.
        let bytes = bytemuck::bytes_of(&proof);
        assert_eq!(bytes.len(), 16);
        assert_eq!(*bytemuck::from_bytes::<ChannelProof>(bytes), proof);

        // HID indexed-string form: the same four fields as text.
        let s = proof.to_hid_string();
        assert_eq!(s, alloc::format!("PFCP:{GAMEPAD_PROTO_VERSION}:2:4242"));
        assert_eq!(ChannelProof::from_hid_string(&s), Some(proof));

        // Every malformed shape parses to None — the host then refuses to deliver.
        for bad in [
            "",
            "PFCP",
            "PFCP:",
            "PFCP:3:0",        // truncated read
            "PFCP:3:0:4242:9", // trailing field we never mint
            "PFCP:3:0:-1",     // not a u32
            "PFCP:3:0:0x10",   // not decimal
            "PFCP:3:0: 4242",  // whitespace is not trimmed away into a valid pid
            "NOPE:3:0:4242",   // another driver answered this string index
            "pfcp:3:0:4242",   // prefix is case-sensitive
        ] {
            assert_eq!(
                ChannelProof::from_hid_string(bad),
                None,
                "malformed proof {bad:?} must not parse"
            );
        }
    }

    /// `check` is the gate that decides whether a pid is allowed to receive a pad's whole input and
    /// rumble surface, so pin each refusal: a foreign driver, a version skew, and — the one that
    /// would silently cross-wire two live pads — an answer from the wrong devnode.
    #[test]
    fn channel_proof_check_refuses_everything_it_should() {
        use gamepad::*;
        assert_eq!(ChannelProof::new(0, 1234).check(0), Ok(1234));
        assert_eq!(ChannelProof::new(3, 1234).check(3), Ok(1234));

        // Right shape, WRONG pad: the interface lookup resolved another pad's devnode.
        assert!(ChannelProof::new(1, 1234).check(0).is_err());
        // A driver that isn't ours answered the reserved string index / IOCTL.
        let mut foreign = ChannelProof::new(0, 1234);
        foreign.magic = 0xDEAD_BEEF;
        assert!(foreign.check(0).is_err());
        // Version skew must fail closed, not "probably compatible".
        let mut old = ChannelProof::new(0, 1234);
        old.proto = GAMEPAD_PROTO_VERSION - 1;
        assert!(old.check(0).is_err());
        // pid 0 is never a duplication target.
        assert!(ChannelProof::new(0, 0).check(0).is_err());
    }

    /// A v2 driver answers no proof at all and a v2 host never asks, so the version must have moved
    /// — this is the tripwire that stops the two halves shipping out of step.
    #[test]
    fn gamepad_proto_is_at_the_channel_proof_version() {
        assert_eq!(gamepad::GAMEPAD_PROTO_VERSION, 3);
    }

    /// The pad identities carry the proof in a HID FEATURE report — the transport chosen because
    /// `0x85` is already declared in the captured descriptors, so nothing about the device's
    /// Steam/SDL-visible identity changes. Pin the framing (report id in byte 0, proof in 1..17,
    /// zero padding to the descriptor's length) and the short-read refusal.
    #[test]
    fn channel_proof_feature_report_round_trips_and_refuses_short_reads() {
        use gamepad::*;
        let proof = ChannelProof::new(1, 4242);
        let rep = proof
            .to_feature_report(HID_FEATURE_REPORT_CHANNEL_PROOF, 64)
            .expect("64 bytes is plenty");
        assert_eq!(rep.len(), 64);
        assert_eq!(
            rep[0], 0x85,
            "byte 0 is the report id, as every HID feature reply is"
        );
        assert!(rep[17..].iter().all(|&b| b == 0), "tail is zero padding");
        assert_eq!(ChannelProof::from_feature_report(&rep), Some(proof));

        // Exactly big enough, and one byte too small.
        assert!(proof.to_feature_report(0x85, 17).is_some());
        assert!(proof.to_feature_report(0x85, 16).is_none());
        // A truncated read must NOT be zero-extended into a pid.
        assert_eq!(ChannelProof::from_feature_report(&rep[..16]), None);
        assert_eq!(ChannelProof::from_feature_report(&[]), None);

        // The Deck's private command is two bytes so a stray Steam command can't collide, and is
        // distinct from the commands the driver already serves.
        assert_eq!(DECK_PROOF_CMD.len(), 2);
        assert!(!DECK_PROOF_CMD.starts_with(&[0x83]) && !DECK_PROOF_CMD.starts_with(&[0xAE]));
        assert!(!DECK_PROOF_CMD.starts_with(&[0xEB]) && !DECK_PROOF_CMD.starts_with(&[0x8F]));
    }

    #[test]
    fn triton_devtype_is_the_next_free_slot() {
        assert_eq!(gamepad::DEVTYPE_TRITON, 7);
    }

    /// The GET reply echoes the LAST SET's command — the Valve query dance Steam's
    /// GetControllerInfo runs; a mismatched command type makes Steam drop the pad
    /// (on-glass 2026-07-15, and the Phase-0 static-zeros run looped forever).
    #[test]
    fn triton_feature_reply_echoes_the_queried_command() {
        // Settings write (lizard-off) reads back as a mirror.
        let set = [0x01, 0x87, 0x03, 0x09, 0x00, 0x00];
        let r = triton::feature_reply(&set, "FVPF130200D03", 0x5452_4900);
        assert_eq!(r[0], 0x01);
        assert_eq!(&r[1..6], &[0x87, 0x03, 0x09, 0x00, 0x00]);
    }

    #[test]
    fn triton_feature_reply_synthesizes_attributes_for_0x83() {
        let set = [0x01, 0x83, 0x00];
        let r = triton::feature_reply(&set, "FVPF130200D03", 0x5452_4900);
        assert_eq!(&r[..3], &[0x01, 0x83, 0x19]); // 25-byte TLV payload
        assert_eq!(r[3], 0x01); // first attribute id: product id
                                // Tag-4 TLV (ATTRIB_FIRMWARE_BUILD_TIME) carries FW_BUILD_TIME — the Bench-1 regression
                                // pin: a stale epoch here makes Steam prompt to "update" the virtual pad's firmware.
        assert_eq!(r[18], 0x04);
        assert_eq!(r[19..23], triton::FW_BUILD_TIME.to_le_bytes());
    }

    #[test]
    fn triton_firmware_info_build_time_agrees_with_the_attributes_reply() {
        let set = [0x01, 0xF2, 0x00, 0x00];
        let r = triton::feature_reply(&set, "FVPF130200D03", 0x5452_4900);
        assert_eq!(&r[..4], &[0x01, 0xF2, 0x29, 0x00]);
        // Bytes 4..8 mirror the 0x83 reply's tag-4 build time — Steam may cross-check the two.
        assert_eq!(r[4..8], triton::FW_BUILD_TIME.to_le_bytes());
    }

    #[test]
    fn triton_input_len_matches_the_descriptor() {
        assert_eq!(triton::input_len(0x42), Some(54));
        assert_eq!(triton::input_len(0x45), Some(46));
        assert_eq!(triton::input_len(0x43), Some(15));
        assert_eq!(triton::input_len(0x44), Some(6));
        assert_eq!(triton::input_len(0x79), Some(2));
        assert_eq!(triton::input_len(0x7B), Some(13));
        assert_eq!(triton::input_len(0x47), None); // BLE-only id, not in the wired descriptor
        assert_eq!(triton::input_len(0x01), None);
    }

    #[test]
    fn triton_out_report_len_matches_the_descriptor_and_bench_table() {
        assert_eq!(triton::out_report_len(0x80), 10);
        assert_eq!(triton::out_report_len(0x81), 8);
        assert_eq!(triton::out_report_len(0x82), 4);
        assert_eq!(triton::out_report_len(0x83), 10);
        assert_eq!(triton::out_report_len(0x84), 9);
        assert_eq!(triton::out_report_len(0x85), 4);
        assert_eq!(triton::out_report_len(0x86), 4);
        assert_eq!(triton::out_report_len(0x87), 64);
        assert_eq!(triton::out_report_len(0x88), 64);
        assert_eq!(triton::out_report_len(0x89), 64);
        // Undeclared ids stay whole (64 = no trim) — never guess a length.
        assert_eq!(triton::out_report_len(0x00), 64);
        assert_eq!(triton::out_report_len(0x8A), 64);
    }

    #[test]
    fn triton_serial_shape_dodges_the_pf_prefix_rejection() {
        let mut s = [0u8; 13];
        triton::serial(3, &mut s);
        assert_eq!(&s, b"FVPF130203D03");
    }

    #[test]
    fn triton_rdesc_is_the_372_byte_capture() {
        assert_eq!(triton::RDESC.len(), 372);
        // Mouse TLC opens it: Usage Page Generic Desktop, Usage Mouse, Collection App, Report ID 0x40.
        assert_eq!(
            &triton::RDESC[..8],
            &[0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x40]
        );
    }

    #[test]
    fn out_feature_bit_round_trips() {
        let tagged = 64u32 | triton::OUT_FEATURE_BIT;
        assert_eq!(triton::out_len(tagged), 64);
        assert!(triton::out_is_feature(tagged));
        assert!(!triton::out_is_feature(64));
        assert_eq!(triton::out_len(64), 64);
    }
}
