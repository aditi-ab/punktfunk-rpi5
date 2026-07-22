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
/// [`control::AddReply::cursor_excluded`] — the driver reports whether the ADDed monitor's OS
/// target already carries a hardware-cursor declare from an earlier session. A declare is
/// IRREVOCABLE for the target's life (remote-desktop-sweep §8.6, proven on-glass): DWM never
/// composites the software cursor back into that target's frames, and the sticky state survives
/// monitor REMOVE→ADD because the host hands every client a STABLE target id. The host uses the
/// flag to self-composite the pointer (GDI poller + blend) in sessions that never negotiate the
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
        /// Non-zero = this monitor's OS target already carries an IRREVOCABLE hardware-cursor
        /// declare from an earlier session (remote-desktop-sweep §8.6): DWM excludes the pointer
        /// from every frame on this target, forever, and a session without the cursor channel must
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
        pub _pad: u32,
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

        assert!(size_of::<SharedHeader>() == 64);
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
    };
}

/// Gamepad shared-memory layouts (host ↔ the UMDF gamepad drivers `pf_xusb` / `pf_dualsense`).
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
    /// Pad section magic — the exact u32 the shipped host + `pf_dualsense` driver compare (loosely
    /// "PFDS"). (Note: the two magics happen to use opposite byte-order mnemonics in the legacy code;
    /// only the u32 value is the contract.)
    pub const PAD_MAGIC: u32 = 0x5046_4453;

    /// `device_type` selector the `pf_dualsense` driver reads to pick its HID identity. The section is
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
    pub const GAMEPAD_PROTO_VERSION: u32 = 2;

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
    /// 3. host polls `driver_pid`, verifies the pid is a genuine WUDFHost, duplicates the unnamed DATA
    ///    section into it, then writes `data_handle` + `handle_pid` and bumps `handle_seq` LAST;
    /// 4. driver sees a fresh `handle_seq` addressed to its own pid, maps `data_handle`, and validates
    ///    the mapped section's magic + `pad_index` before use.
    ///
    /// Deliberately safe to leave named + LS-openable: it carries only pids (not sensitive) and a
    /// handle VALUE (meaningless outside the target WUDFHost's handle table). A sibling LocalService
    /// that tampers with it can at worst mis-route a delivery — a gamepad DoS, never a read or an
    /// injection (it cannot place a valid section handle in the WUDFHost, and the driver's
    /// magic+`pad_index` validation rejects any handle that doesn't resolve to this pad's section).
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

    /// Output-report ring depth. 8 slots at the host's ~4 ms poll tolerates a sustained 2 kHz
    /// writer — double any real HID output rate.
    pub const OUT_RING_LEN: u32 = 8;
    pub const OUT_RING_LEN_USIZE: usize = OUT_RING_LEN as usize;

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

    /// Virtual DualSense / DualShock 4 shared section (1024 B; bytes `0..256` are the v2 legacy
    /// layout verbatim — [`PAD_SHM_LEGACY_SIZE`]). The host writes the `0x01`-style HID input
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
        /// Driver-bumped (AFTER writing `out_ring[ring_head % OUT_RING_LEN]`) count of reports
        /// ever published to the ring — the host's drain cursor compares against its own tail and
        /// detects overflow by `head - tail > OUT_RING_LEN`. Same publish-then-bump store order as
        /// `out_seq` (the host's Acquire load orders the reads). Carved from v2 reserved space
        /// (v2.1).
        pub ring_head: u32,
        pub _reserved1: [u8; 92],
        /// The lossless output-report ring (v2.1) — see the struct docs and [`OutSlot`].
        pub out_ring: [OutSlot; OUT_RING_LEN_USIZE],
        pub _reserved2: [u8; 224],
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

        assert!(size_of::<PadShm>() == 1024);
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

    #[test]
    fn shared_header_is_pod_and_64_bytes() {
        let mut h = frame::SharedHeader::zeroed();
        h.magic = frame::MAGIC;
        h.width = 5120;
        h.height = 1440;
        h.target_id = 262;
        let bytes = bytemuck::bytes_of(&h);
        assert_eq!(bytes.len(), 64);
        let back: frame::SharedHeader = *bytemuck::from_bytes(bytes);
        assert_eq!(back.magic, frame::MAGIC);
        assert_eq!(back.width, 5120);
        assert_eq!(back.height, 1440);
        // v3: the monitor binding occupies the old `_pad` slot at offset 28 — byte-compatible (a v2
        // host left it zero there).
        assert_eq!(bytes[28..32], 262u32.to_le_bytes());
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
        let old = *bytemuck::from_bytes::<control::AddRequest>(&legacy);
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
            _pad: 0,
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
}
