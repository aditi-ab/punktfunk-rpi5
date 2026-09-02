//! STEP 6 — IDD-push frame publisher (DRIVER side), attached over the **sealed channel**.
//!
//! The restricted WUDFHost token canNOT create named kernel objects — and since the frame channel
//! carries whole-desktop pixels, the objects are not merely host-created but **unnamed**: nothing to
//! enumerate, open by name, or pre-create ("squat"). The **host** creates the shared header +
//! frame-ready event + ring of keyed-mutex textures with no names, duplicates the handles INTO this
//! WUDFHost process (`DuplicateHandle` — SYSTEM can, we can't reciprocate, which is why the host is the
//! broker), and delivers the handle VALUES over `IOCTL_SET_FRAME_CHANNEL` ([`crate::control`] stashes
//! them per monitor as a [`FrameChannel`]). The swap-chain worker turns the delivery into the
//! monitor-owned [`RingEndpoint`] (mapping + retained handles, outlives any one worker) and opens its
//! own device-bound [`FramePublisher`] on it ([`FramePublisher::open`]). Only the two endpoint
//! processes ever hold a handle to any frame object — see `design/idd-push-security.md`.
//!
//! The driver writes its render-adapter LUID + a status code back into the host-created header (our
//! only driver-visibility channel), then copies each acquired swap-chain surface into the next ring
//! slot and signals the host. Host counterpart: `crates/pf-capture/src/windows/idd_push.rs`. The
//! `SharedHeader` layout, [`FrameToken`] packing, `MAGIC`/`RING_LEN`, `DRV_STATUS_*` codes and the
//! channel-delivery struct all come from `pf_driver_proto`, which OWNS the contract (with `const`
//! size asserts so any drift is a compile error).
//!
//! This module also owns the [`FrameStash`] — the driver-retained copy of the most recent composed
//! frame that the swap-chain worker republishes into every freshly-attached ring, so a session
//! opening onto an IDLE desktop gets its first frame immediately (see the type docs).

use std::mem::offset_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use pf_driver_proto::control::{SetFrameChannelRequest, SetFrameChannelRequestV2};
use pf_driver_proto::frame::fence::{self, Claim, SlotRecord};
use pf_driver_proto::frame::{
    AttachReject, CAP_FENCE_RING, CAP_RING_HEALTH_V3, CAP_SOURCE_SEQUENCE_QPC,
    DRV_STATUS_BIND_FAIL, DRV_STATUS_NO_DEVICE1, DRV_STATUS_OPENED, DRV_STATUS_TEX_FAIL,
    ERR_DOMAIN_DEVICE, ERR_DOMAIN_TRANSPORT, FrameToken, HealthState, RING_LEN, SharedHeader,
    VERSION_TELEMETRY, check_attach, negotiate, pack_opened_detail, v3_readable,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11Device1, ID3D11Device5,
    ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;
use windows::Win32::System::Memory::{
    FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::System::Threading::SetEvent;
use windows::core::Interface;

/// `WAIT_TIMEOUT` as an HRESULT — `AcquireSync` returns this when the slot is held by the consumer.
/// SUCCESS-severity (positive), so the windows-rs `Result` wrapper can never surface it (`.ok()` maps
/// every non-negative HRESULT to `Ok(())`) — the publish loop reads the raw vtable HRESULT instead.
const WAIT_TIMEOUT_HRESULT: i32 = 0x0000_0102;
/// The capability bits this driver ALWAYS advertises (`frame::CAP_*`). `CAP_FENCE_RING` is added
/// per endpoint once its shared fences opened on the worker's device ([`FramePublisher::open`]);
/// endpoint survival and the swap-chain reset actuator arrive with the WP5 live gate / WP13.
const DRIVER_CAPABILITIES: u32 = CAP_RING_HEALTH_V3 | CAP_SOURCE_SEQUENCE_QPC;

/// The current QPC tick (0 if the counter is unavailable — it cannot be on any OS we load on).
fn qpc_now() -> u64 {
    let mut qpc = 0i64;
    // SAFETY: plain FFI; `qpc` is a valid local out-param.
    match unsafe { QueryPerformanceCounter(&mut qpc) } {
        Ok(()) => qpc as u64,
        Err(_) => 0,
    }
}

/// `WAIT_ABANDONED` as an HRESULT — the host died while holding the slot's keyed mutex. Also
/// SUCCESS-severity, and ownership DID transfer to the caller.
const WAIT_ABANDONED_HRESULT: i32 = 0x0000_0080;

/// One monitor's sealed-channel bootstrap: the handle VALUES the host duplicated into THIS process
/// (`IOCTL_SET_FRAME_CHANNEL`). Owning a `FrameChannel` means owning those handles — exactly one of
/// {the monitor stash ([`crate::monitor`]), a [`FramePublisher`] under construction} holds it at any
/// time, and `Drop` closes every entry not consumed, so a replaced/unmatched/failed delivery can never
/// leak entries in the WUDFHost handle table. A `0` field means "taken" (or never valid) and is skipped.
pub struct FrameChannel {
    /// The ring generation these textures belong to (checked against the header at attach).
    generation: u32,
    ring_len: u32,
    /// Section size the host declared (v3; 0 from a pre-v3 host) — half of the v3-tail gate.
    header_bytes: u32,
    header: u64,
    event: u64,
    textures: [u64; RING_LEN as usize],
    /// Shared `ID3D11Fence` handles from a v2 request (WP7); both 0 from a v1 request or a host
    /// that negotiated the keyed-mutex arm.
    ready_fence: u64,
    retire_fence: u64,
}

impl FrameChannel {
    /// Validate + adopt the handle values from the host's IOCTL. `None` on a malformed request (bad
    /// `ring_len`, zero handles) — the caller completes with `STATUS_INVALID_PARAMETER` and nothing is
    /// adopted (a zero value is never treated as a handle).
    pub fn from_request(req: &SetFrameChannelRequest) -> Option<Self> {
        if req.ring_len == 0 || req.ring_len > RING_LEN {
            return None;
        }
        if req.header_handle == 0
            || req.event_handle == 0
            || req.texture_handles[..req.ring_len as usize].contains(&0)
        {
            return None;
        }
        Some(Self {
            generation: req.generation,
            ring_len: req.ring_len,
            header_bytes: req.header_bytes,
            header: req.header_handle,
            event: req.event_handle,
            textures: req.texture_handles,
            ready_fence: 0,
            retire_fence: 0,
        })
    }

    /// [`Self::from_request`] for the v2 request: the fences come as a pair or not at all (one of
    /// two is malformed — nothing adopted).
    pub fn from_request_v2(req: &SetFrameChannelRequestV2) -> Option<Self> {
        if (req.ready_fence_handle == 0) != (req.retire_fence_handle == 0) {
            return None;
        }
        let mut me = Self::from_request(&req.v1)?;
        me.ready_fence = req.ready_fence_handle;
        me.retire_fence = req.retire_fence_handle;
        Some(me)
    }

    /// Move a handle value out of the channel: the caller now owns it; `Drop` skips the zeroed slot.
    fn take(v: &mut u64) -> HANDLE {
        HANDLE(core::mem::take(v) as usize as *mut core::ffi::c_void)
    }

    /// Disarm without closing anything — for the adopt-on-success-only contract: a delivery rejected
    /// with an error completion was never adopted, and the HOST reaps its remote duplicates on that
    /// error, so closing here too would double-close (see `crate::control::set_frame_channel`).
    pub fn into_unowned(mut self) {
        self.header = 0;
        self.event = 0;
        self.textures = [0; RING_LEN as usize];
        self.ready_fence = 0;
        self.retire_fence = 0;
    }
}

impl Drop for FrameChannel {
    fn drop(&mut self) {
        for v in [
            &mut self.header,
            &mut self.event,
            &mut self.ready_fence,
            &mut self.retire_fence,
        ]
        .into_iter()
        .chain(self.textures.iter_mut())
        {
            if *v != 0 {
                let h = Self::take(v);
                // SAFETY: `h` is a live handle the host duplicated into this process for us to own; it
                // was not consumed (non-zero), so this is its sole close.
                unsafe {
                    let _ = CloseHandle(h);
                }
            }
        }
    }
}

// NB: `FrameChannel` is plain integers, so it is auto-`Send` — it crosses from the control-plane
// dispatch thread (stash) to the swap-chain worker (attach) with `MONITOR_MODES` serializing the
// hand-off; no manual impl needed (handle values are process-global tokens, not thread-affine).

/// The MONITOR-owned half of an attached ring (immunity plan D4 / WP5): the mapped header, the
/// frame-ready event, the RETAINED shared-texture NT handles, the ring's identity (generation,
/// v3 gate) and the sequences that must outlive any one worker. Every swap-chain assignment opens
/// its own device-bound [`FramePublisher`] on it; worker exit drops those COM objects, and nothing
/// device-bound ever crosses to the next assignment. Held as an `Arc` by the monitor entry and by
/// the live publisher — the last holder unmaps and closes.
pub struct RingEndpoint {
    map: HANDLE,
    header: *mut SharedHeader,
    event: HANDLE,
    /// Retained (NOT closed after open, unlike the pre-WP5 attach): each assignment re-opens them
    /// on its own device. Closed by `Drop`.
    textures: [u64; RING_LEN as usize],
    ring_len: u32,
    /// The ring generation this endpoint attached to — see [`Self::is_stale`].
    generation: u32,
    /// The host built the telemetry-capable (v2, 88-byte) layout — gates the v2 tail writes.
    telemetry: bool,
    /// The v3 ring-health tail may be touched (`frame::v3_readable` over version AND declared size).
    v3: bool,
    /// The v4 slot table exists AND the host delivered both shared fences (`fence::v4_readable`
    /// plus the handles). Whether the FENCE PROTOCOL runs is decided per open, by negotiation.
    v4: bool,
    /// Retained shared-fence NT handles (0 = none); each worker opens them on its own device.
    ready_fence: u64,
    retire_fence: u64,
    /// Producer-ready fence value, monotonic per ring generation across worker re-opens.
    ready_value: AtomicU64,
    /// Publish-token sequence, monotonic per ring generation across worker re-opens.
    seq: AtomicU64,
    /// Publishes that landed / frames dropped, mirrored into the v3 tail (monotonic per ring).
    published_total: AtomicU64,
    dropped_total: AtomicU64,
    /// The MONITOR's source sequence (D2: monotonic across ring rebuilds) — shared with the entry.
    source_seq: Arc<AtomicU64>,
}

// SAFETY: the raw header pointer is a mapped section alive until `Drop`; every cross-thread access
// to it goes through atomics (or plain status writes serialized by "one worker per monitor").
unsafe impl Send for RingEndpoint {}
// SAFETY: as above — shared references only reach atomic views of the mapping.
unsafe impl Sync for RingEndpoint {}

impl RingEndpoint {
    /// Map + validate a delivered [`FrameChannel`], consuming it. Device-independent: no D3D here.
    /// On ANY failure every handle is closed (the partially-built endpoint's `Drop`, or the
    /// channel's) and the host re-delivers on its next recreate — failure is terminal for THIS
    /// delivery. `target_id` is the OWNING monitor's OS target id: the mapped ring must name it
    /// (proto v3 binding validation), so a cross-delivered ring can never carry this monitor's
    /// frames into another client's stream.
    pub fn from_channel(
        mut channel: FrameChannel,
        target_id: u32,
        source_seq: Arc<AtomicU64>,
    ) -> windows::core::Result<Self> {
        // 1. Map the header from the duplicated section handle (ours from here on).
        let map = FrameChannel::take(&mut channel.header);
        // SAFETY: `map` is the live section handle the host duplicated into this process; a byte
        // count of 0 maps the WHOLE section — always exactly what the host built, whatever version
        // (the `telemetry`/`v3` gates keep our writes inside it). Read/write only: the host
        // duplicates the handle with `SECTION_MAP_READ | SECTION_MAP_WRITE`, so a wider request
        // would fail. The null `view.Value` is checked below.
        let view = unsafe { MapViewOfFile(map, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0) };
        if view.Value.is_null() {
            let err = windows::core::Error::from_win32();
            // SAFETY: `map` is the taken section handle, closed once here on the error path (the
            // rest of `channel` closes via its Drop).
            unsafe {
                let _ = CloseHandle(map);
            }
            return Err(err);
        }
        let header = view.Value.cast::<SharedHeader>();
        // From here `me`'s Drop owns cleanup on every early return.
        let mut me = Self {
            map,
            header,
            event: FrameChannel::take(&mut channel.event),
            textures: core::mem::take(&mut channel.textures),
            ring_len: channel.ring_len,
            generation: 0,
            telemetry: false,
            v3: false,
            v4: false,
            ready_fence: core::mem::take(&mut channel.ready_fence),
            retire_fence: core::mem::take(&mut channel.retire_fence),
            ready_value: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            published_total: AtomicU64::new(0),
            dropped_total: AtomicU64::new(0),
            source_seq,
        };

        // 2. Validate (`check_attach`, unit-tested in pf-driver-proto — staleness first, binding
        //    second): the channel's generation must match the header's CURRENT one (else a fresh
        //    delivery is coming), and the ring must NAME THIS MONITOR (a cross-wire fails CLOSED).
        // SAFETY: `header` is the mapped host header; `magic`/`generation` are read Acquire to pair
        // with the host's Release publishes; `target_id`/`version` are plain in-bounds reads.
        let (magic, header_gen, header_target, header_version) = unsafe {
            (
                (*(core::ptr::addr_of!((*header).magic) as *const AtomicU32))
                    .load(Ordering::Acquire),
                (*(core::ptr::addr_of!((*header).generation) as *const AtomicU32))
                    .load(Ordering::Acquire),
                (*header).target_id,
                (*header).version,
            )
        };
        match check_attach(
            magic,
            header_gen,
            header_target,
            channel.generation,
            target_id,
        ) {
            Ok(()) => {}
            Err(AttachReject::Stale) => {
                dbglog!(
                    "[pf-vd] frame-push(driver): dropping channel delivery (channel gen {} vs header gen {header_gen}) — superseded",
                    channel.generation
                );
                // E_BOUNDS — stand-in for "stale delivery"; the caller only drops the attempt.
                return Err(windows::core::HRESULT(0x8000_000Bu32 as i32).into());
            }
            Err(AttachReject::BindMismatch) => {
                dbglog!(
                    "[pf-vd] frame-push(driver): REFUSING attach — ring names target {header_target}, this monitor is {target_id} (host stash cross-wire?)"
                );
                // Report the refusal through the header so the host's wait_for_attach fails the
                // open LOUDLY (DRV_STATUS_BIND_FAIL) instead of timing out mute; the detail carries
                // the target id the ring claims.
                // SAFETY: `header` is the live mapped view; in-bounds scalar writes.
                unsafe {
                    (*header).driver_status_detail = header_target;
                    (*header).driver_status = DRV_STATUS_BIND_FAIL;
                }
                // E_INVALIDARG — the delivery itself is wrong; the caller only drops the attempt.
                return Err(windows::core::HRESULT(0x8007_0057u32 as i32).into());
            }
        }
        let v3 = v3_readable(header_version, channel.header_bytes);
        let v4 = fence::v4_readable(header_version, channel.header_bytes)
            && me.ready_fence != 0
            && me.retire_fence != 0;
        dbglog!(
            "[pf-vd] frame-push(driver): ring endpoint mapped for gen {header_gen} ({} slots, target {target_id}, v3_tail={v3}, fences={v4})",
            me.ring_len
        );
        me.generation = header_gen;
        me.telemetry = header_version >= VERSION_TELEMETRY;
        me.v3 = v3;
        me.v4 = v4;
        // v3: advertise what this driver can do. The state word follows from `stamp_epochs`
        // (Release, after the epochs) on each worker open; `CAP_FENCE_RING` joins once a worker
        // actually opened the fences (`FramePublisher::open`).
        me.v3_store_u32(offset_of!(SharedHeader, capabilities), DRIVER_CAPABILITIES);
        Ok(me)
    }

    /// The ring generation this endpoint attached to.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Whether the fence protocol is NEGOTIATED for this ring: the host built a v4 ring with fences
    /// and advertised `CAP_FENCE_RING`, and this driver advertised it back (fences opened).
    fn fence_negotiated(&self) -> bool {
        if !self.v4 {
            return false;
        }
        // SAFETY: the v4 gate implies the v3 tail; both words are naturally-aligned u32s in it.
        let (host, drv) = unsafe {
            (
                (*self
                    .header
                    .cast::<u8>()
                    .add(offset_of!(SharedHeader, host_capabilities))
                    .cast::<AtomicU32>())
                .load(Ordering::Acquire),
                (*self
                    .header
                    .cast::<u8>()
                    .add(offset_of!(SharedHeader, capabilities))
                    .cast::<AtomicU32>())
                .load(Ordering::Acquire),
            )
        };
        negotiate(host, drv) & CAP_FENCE_RING != 0
    }

    /// Atomic view of slot `i`'s `state` word in the v4 slot table.
    fn slot_state(&self, i: usize) -> &AtomicU32 {
        // SAFETY: `v4` (checked by every caller's mode) proves the host built + declared the v4
        // layout; `slot_offset(i) + offset_of!(state)` is a naturally-aligned u32 inside it.
        unsafe {
            &*self
                .header
                .cast::<u8>()
                .add(fence::slot_offset(i) + offset_of!(SlotRecord, state))
                .cast::<AtomicU32>()
        }
    }

    /// Atomic view of a `u64` field of slot `i`'s record (`off` = `offset_of!(SlotRecord, ..)`).
    fn slot_u64(&self, i: usize, off: usize) -> &AtomicU64 {
        // SAFETY: as `slot_state`, for an 8-aligned u64 field.
        unsafe {
            &*self
                .header
                .cast::<u8>()
                .add(fence::slot_offset(i) + off)
                .cast::<AtomicU64>()
        }
    }

    /// True once the host has recreated the ring (bumped the header generation) — e.g. the display's
    /// HDR mode flipped, so the ring format changed (FP16 ⇄ BGRA) and a fresh channel delivery is
    /// coming. The worker drops its publisher on this so it re-attaches to the new ring.
    pub fn is_stale(&self) -> bool {
        // SAFETY: `self.header` stays mapped for the endpoint's lifetime; `generation` lives within
        // it and is read atomically (Acquire) to pair with the host's Release bump on a recreate.
        let cur = unsafe {
            (*(core::ptr::addr_of!((*self.header).generation) as *const AtomicU32))
                .load(Ordering::Acquire)
        };
        cur != self.generation
    }

    /// Store one v3 tail `u32` at byte offset `off` (Relaxed; the state word carries the Release) —
    /// a no-op unless the v3 gate passed, so a v2 section is never touched past byte 88.
    fn v3_store_u32(&self, off: usize, v: u32) {
        if !self.v3 {
            return;
        }
        // SAFETY: the v3 gate proves the host built and declared the 152-byte layout; `off` is an
        // `offset_of!` of a naturally-aligned u32 inside it (the `latest_cell` pattern).
        unsafe {
            (*self.header.cast::<u8>().add(off).cast::<AtomicU32>()).store(v, Ordering::Relaxed)
        }
    }

    /// Store one v3 tail `u64` — same contract as [`Self::v3_store_u32`].
    fn v3_store_u64(&self, off: usize, v: u64) {
        if !self.v3 {
            return;
        }
        // SAFETY: as `v3_store_u32`, for a naturally-aligned u64 field.
        unsafe {
            (*self.header.cast::<u8>().add(off).cast::<AtomicU64>()).store(v, Ordering::Relaxed)
        }
    }

    /// Publish a [`HealthState`] LAST, with Release, so a host that Acquire-loads it sees every
    /// epoch/sequence/error field written before it (the v3 snapshot contract).
    fn v3_set_state(&self, state: HealthState) {
        if !self.v3 {
            return;
        }
        // SAFETY: as `v3_store_u32`; Release is the ordering the contract names.
        unsafe {
            (*self
                .header
                .cast::<u8>()
                .add(offset_of!(SharedHeader, health_state))
                .cast::<AtomicU32>())
            .store(state as u32, Ordering::Release);
        }
    }

    /// Stamp the epochs the opening worker runs under and mark the ring ACTIVE — `assignment`
    /// changes per swap-chain assignment, `device` per D3D device object (`Direct3DDevice::epoch`).
    fn stamp_epochs(&self, assignment: u32, device: u32) {
        self.v3_store_u32(offset_of!(SharedHeader, assignment_epoch), assignment);
        self.v3_store_u32(offset_of!(SharedHeader, device_epoch), device);
        self.v3_set_state(HealthState::Active);
    }

    /// The worker is retiring its publisher (generation superseded / worker exit) and a fresh open
    /// is expected — tell the host so a quiet ring reads as REBUILDING, not stalled.
    pub fn mark_rebuilding(&self) {
        self.v3_set_state(HealthState::Rebuilding);
    }

    /// The generation is poisoned: record the cause, then DEAD (Release, last).
    fn mark_dead(&self, domain: u32, code: i32) {
        self.v3_store_u32(offset_of!(SharedHeader, terminal_error_domain), domain);
        self.v3_store_u32(offset_of!(SharedHeader, terminal_error_code), code as u32);
        self.v3_set_state(HealthState::Dead);
    }

    /// One more frame dropped (busy / mismatch / fatal) — the v3 counter the host deltas.
    fn note_drop(&self) {
        let n = self
            .dropped_total
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        self.v3_store_u64(offset_of!(SharedHeader, dropped_total), n);
    }

    #[inline]
    fn latest_cell(&self) -> &AtomicU64 {
        // SAFETY: `self.header` stays mapped for the endpoint's lifetime (unmapped only in Drop);
        // the `latest` field lives within it and is naturally aligned, so this view is valid.
        unsafe { &*(core::ptr::addr_of!((*self.header).latest) as *const AtomicU64) }
    }
}

impl Drop for RingEndpoint {
    fn drop(&mut self) {
        // Every publisher (opened textures + keyed mutexes) is gone — they hold an `Arc` to us.
        // Unmap the header, then close the event, section and every retained texture handle:
        // nothing of the channel outlives the endpoint (`design/idd-push-security.md`).
        // SAFETY: drop runs once; `self.header` is the live mapped view and every non-zero handle
        // is one this endpoint owns — each unmapped/closed exactly once here.
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.header.cast(),
            });
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.map);
            for v in self
                .textures
                .iter_mut()
                .chain([&mut self.ready_fence, &mut self.retire_fence])
            {
                if *v != 0 {
                    let _ = CloseHandle(FrameChannel::take(v));
                }
            }
        }
    }
}

struct Slot {
    tex: ID3D11Texture2D,
    /// The keyed-mutex arm's lock; `None` on a fence-mode ring (the host creates those textures
    /// without `SHARED_KEYEDMUTEX`).
    mutex: Option<IDXGIKeyedMutex>,
}

/// The worker's opened fence objects (fence mode only).
struct Fences {
    ctx4: ID3D11DeviceContext4,
    ready: ID3D11Fence,
    retire: ID3D11Fence,
}

/// The driver-retained copy of the most recent composed frame — the FIRST-FRAME GUARANTEE.
///
/// DWM presents a display only when something DIRTIES it, so a ring freshly attached over an idle
/// desktop could wait forever for its first frame (session open onto a static desktop = black
/// stream until something repaints). DXGI Desktop Duplication never had this problem because the
/// OS seeds a new duplication with the CURRENT desktop image; this stash reconstructs that
/// guarantee for the IDD-push path. The swap-chain worker copies into it every composed frame it
/// canNOT deliver to a live ring (no publisher attached, or a size/format-mismatched surface
/// racing the host's ring recreate) and HARVESTS a superseded publisher's last-published slot, so
/// at every attach the freshest desktop image is republished into the new ring immediately — no
/// compose to wait for, no synthetic-input "kick" needed on the host.
///
/// Costs nothing at steady state: a matched publish goes ONLY into the ring, and between sessions
/// the still-attached publisher keeps writing the (dead) previous ring, which the harvest then
/// reads — the extra `CopyResource` happens only for unattached/mismatched frames, which are
/// damage-driven and rare.
pub struct FrameStash {
    /// Lazily (re)created at the source's size/format: a plain default-usage texture (no bind or
    /// misc flags — a pure copy source/target) on the worker's pooled device.
    tex: Option<ID3D11Texture2D>,
    /// When the retained content was captured (monotonic). Harvest only overwrites OLDER content,
    /// so a superseded publisher's last frame can never clobber a fresher mismatch-stashed surface
    /// (e.g. the first FP16 frames of an HDR flip stashed while the ring was still BGRA).
    stored_at: Option<Instant>,
}

// SAFETY: like `FramePublisher` — created and used only on the swap-chain worker thread; the
// preserved hand-off across workers is serialized by the monitor stash's Mutex.
unsafe impl Send for FrameStash {}

impl Default for FrameStash {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameStash {
    pub const fn new() -> Self {
        Self {
            tex: None,
            stored_at: None,
        }
    }

    /// The retained frame, if any content has been stored.
    pub fn texture(&self) -> Option<&ID3D11Texture2D> {
        if self.stored_at.is_some() {
            self.tex.as_ref()
        } else {
            None
        }
    }

    /// When the retained content was captured (`None` = empty).
    pub fn stored_at(&self) -> Option<Instant> {
        self.stored_at
    }

    /// Copy `src` into the stash — (re)creating the stash texture if the size/format differ — and
    /// stamp `at` as the content's capture instant. Best-effort: a failed texture create leaves the
    /// stash empty (the attach republish then simply has nothing, which is the old behavior).
    ///
    /// The CALLER owns `src`'s synchronization: a ring slot's keyed mutex must be held across this
    /// call (harvest), and a swap-chain surface is exclusively ours pre-`FinishedProcessingFrame`.
    pub fn store(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        at: Instant,
    ) {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `src` is a live texture per the caller's contract; `desc` is a valid local out-param.
        unsafe { src.GetDesc(&mut desc) };
        let matches = self.tex.as_ref().is_some_and(|t| {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: `t` is the live stash texture; `d` is a valid local out-param.
            unsafe { t.GetDesc(&mut d) };
            d.Width == desc.Width && d.Height == desc.Height && d.Format == desc.Format
        });
        if !matches {
            self.tex = None;
            self.stored_at = None;
            // Struct-update from `default()` so the flag fields keep their zero default whatever
            // their windows-crate type — a copy-only texture wants no bind/misc/CPU flags (in
            // particular NOT the source's SHARED_KEYEDMUTEX, which would gate every copy).
            let make = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                ..Default::default()
            };
            let mut t: Option<ID3D11Texture2D> = None;
            // SAFETY: `device` is the worker's live pooled device; `make` is a fully-initialized
            // local desc and `t` a valid out-param, checked below (best-effort on failure).
            if unsafe { device.CreateTexture2D(&make, None, Some(&mut t)) }.is_err() {
                return;
            }
            self.tex = t;
        }
        if let Some(t) = self.tex.as_ref() {
            // SAFETY: `t` and `src` are live, size/format-matched textures on the same (pooled)
            // device; the pooled immediate context is multithread-protected (`Direct3DDevice`).
            unsafe { context.CopyResource(t, src) };
            self.stored_at = Some(at);
        }
    }
}

/// What [`FramePublisher::publish`] did with a surface — the worker feeds the [`FrameStash`] on
/// the outcomes where the ring did NOT take the frame.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// Copied into a slot + signaled the host.
    Published,
    /// The surface's size/format doesn't match the ring (a display mode-set / HDR flip racing the
    /// host's ring recreate) — worth stashing: it is the freshest desktop image, in exactly the
    /// descriptor the recreated ring will want.
    DescMismatch,
    /// Dropped without publishing (no ring) — nothing to retain.
    Dropped,
    /// Every slot's keyed mutex was host-held — the designed backpressure (the host is alive and
    /// behind). Distinct from the fatal outcomes: retrying next compose is correct.
    AllSlotsBusy,
    /// A slot's keyed mutex came back `WAIT_ABANDONED`: the host died holding it, so the slot
    /// surface's consistency is unknown. No pixels were touched; the caller must stop using this
    /// generation (the next channel delivery attaches a fresh one).
    HostAbandoned,
    /// A fatal synchronization/device HRESULT (device removed, invalid call), or a failed
    /// `ReleaseSync` after the copy — `latest` was NOT advanced. The caller must stop using this
    /// generation.
    Fatal,
}

/// The WORKER-owned half of an attached ring: the slot textures + keyed mutexes opened on THIS
/// worker's device, publishing into the monitor's [`RingEndpoint`]. Created per swap-chain
/// assignment by [`Self::open`] and dropped with the worker — no COM object from one device epoch
/// is reachable from another (immunity plan D4 acceptance).
pub struct FramePublisher {
    ep: Arc<RingEndpoint>,
    context: ID3D11DeviceContext,
    slots: Vec<Slot>,
    /// `Some` = the fence protocol is negotiated for this ring and the fences opened on this
    /// worker's device; `None` = the keyed-mutex arm.
    fences: Option<Fences>,
    next: u32,
    /// The host-created ring textures' DXGI format (from the shared header). A swap-chain surface whose
    /// format differs (e.g. an FP16 HDR frame vs a BGRA ring) is dropped in `publish` — `CopyResource`
    /// needs matching formats.
    ring_format: u32,
    /// Set when a surface is dropped for a descriptor mismatch (a game mode-set the display), cleared on a
    /// matched publish — throttles the drop log to once per mismatch episode (game-capture bug GB1).
    mismatch_logged: bool,
    /// Live diagnostic counters mirrored into `SharedHeader::driver_status_detail` after every
    /// `publish()` (see proto `pack_opened_detail`): surfaces OFFERED to the ring, and how many of
    /// those were DROPPED for a descriptor mismatch. What lets the host's first-frame timeout tell
    /// "DWM never composed" from "every compose mismatched the ring".
    offered: u32,
    mismatch_drops: u32,
    /// Full-width wrapping sibling of `offered` (which packs to 15 saturating bits) — mirrored
    /// into `SharedHeader::offered_total` so the host can delta it across a stall window.
    offered_total: u64,
    /// The slot of the most recent successful publish + when it happened — what [`Self::harvest_into`]
    /// reads when this publisher is superseded. `None` until the first publish.
    last_published: Option<(u32, Instant)>,
}

// SAFETY: created and used only on the swap-chain processor thread.
unsafe impl Send for FramePublisher {}

impl FramePublisher {
    /// Open the endpoint's ring textures on THIS worker's device and mark the ring ACTIVE under
    /// `assignment_epoch` / `device_epoch`. Fails (status reported through the header) when the
    /// device lacks `ID3D11Device1` or a texture will not open here — most likely a render-adapter
    /// mismatch (the host made the textures on a different GPU than the swap-chain renders on).
    /// The endpoint survives a failed open; the caller decides whether to keep it.
    pub fn open(
        ep: Arc<RingEndpoint>,
        render_luid_low: u32,
        render_luid_high: i32,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        assignment_epoch: u32,
        device_epoch: u32,
    ) -> windows::core::Result<Self> {
        let header = ep.header;
        // Report our render adapter to the host first (lets it detect a mismatch).
        // SAFETY: `header` is the endpoint's live mapped header; scalar in-bounds writes.
        unsafe {
            (*header).driver_render_luid_low = render_luid_low;
            (*header).driver_render_luid_high = render_luid_high;
        }
        let device1: ID3D11Device1 = match device.cast() {
            Ok(d) => d,
            Err(e) => {
                // SAFETY: as above — a status write within the mapped header.
                unsafe { (*header).driver_status = DRV_STATUS_NO_DEVICE1 };
                return Err(e);
            }
        };
        // WP7: open the shared fences when the host delivered them. Success is what advertises
        // `CAP_FENCE_RING`; the protocol then runs only if the HOST advertised it too (a v4 host
        // that built a keyed-mutex probe ring delivers fences but leaves the bit clear, so the
        // first ring stays on the mutex arm while the capability is learned).
        let fences = ep
            .v4
            .then(|| Self::open_fences(&ep, device, context))
            .flatten();
        let fail_tex = |e: windows::core::Error| {
            // SAFETY: status writes within the mapped header.
            unsafe {
                (*header).driver_status = DRV_STATUS_TEX_FAIL;
                (*header).driver_status_detail = e.code().0 as u32;
            }
            e
        };
        let mut slots = Vec::with_capacity(ep.ring_len as usize);
        // The NT handles stay RETAINED on the endpoint (closed only by its Drop): the next
        // assignment opens them again on its own device.
        for &tex_handle in ep.textures.iter().take(ep.ring_len as usize) {
            let h = HANDLE(tex_handle as usize as *mut core::ffi::c_void);
            // SAFETY: `device1` is a live ID3D11Device1; `h` is the duplicated shared NT handle for
            // this ring texture, alive for the endpoint's lifetime.
            let opened: windows::core::Result<ID3D11Texture2D> =
                unsafe { device1.OpenSharedResource1(h) };
            let tex = opened.map_err(fail_tex)?;
            // A fence-mode texture carries no keyed mutex; a mutex-mode one must.
            slots.push(Slot {
                mutex: tex.cast::<IDXGIKeyedMutex>().ok(),
                tex,
            });
        }
        if fences.is_some() {
            ep.v3_store_u32(
                offset_of!(SharedHeader, capabilities),
                DRIVER_CAPABILITIES | CAP_FENCE_RING,
            );
        }
        let fence_mode = fences.is_some() && ep.fence_negotiated();
        if !fence_mode && slots.iter().any(|s| s.mutex.is_none()) {
            // The host built a fence ring but the protocol did not negotiate (no fences on this
            // device): there is no lock to run the mutex arm with. Fail the open loudly; the host
            // reads the cleared capability and rebuilds on the mutex arm.
            dbglog!(
                "[pf-vd] frame-push(driver): fence-mode ring without a negotiated fence protocol — refusing (host rebuilds on the keyed-mutex arm)"
            );
            return Err(fail_tex(
                windows::core::HRESULT(0x8007_0032u32 as i32).into(),
            ));
        }
        // Stamp the LIVE diagnostic word BEFORE the status flip, so a host that reads OPENED can
        // trust the detail field is ours (zero counters = "attached, nothing offered yet" — the
        // host's wait-for-attach uses this to tell a never-composed display from a pre-detail
        // driver). Plain best-effort writes, same contract as `driver_status` itself.
        // SAFETY: `header` is the mapped host header; the status/detail fields live within it.
        unsafe {
            (*header).driver_status_detail = pack_opened_detail(0, 0);
            (*header).driver_status = DRV_STATUS_OPENED;
        }
        // v3 epochs go in BEFORE the first publish, so the host never reads an ACTIVE ring without
        // knowing which device/assignment its frames come from.
        ep.stamp_epochs(assignment_epoch, device_epoch);
        dbglog!(
            "[pf-vd] frame-push(driver): opened ring gen {} on device epoch {device_epoch} (assignment {assignment_epoch}, {} slots, {})",
            ep.generation,
            slots.len(),
            if fence_mode {
                "fence protocol"
            } else {
                "keyed-mutex arm"
            }
        );
        Ok(Self {
            // SAFETY: `header` is the mapped host header; `dxgi_format` lives within it.
            ring_format: unsafe { (*header).dxgi_format },
            ep,
            context: context.clone(),
            slots,
            fences: fence_mode.then_some(fences).flatten(),
            next: 0,
            mismatch_logged: false,
            offered: 0,
            mismatch_drops: 0,
            offered_total: 0,
            last_published: None,
        })
    }

    /// Open the endpoint's shared fences on this device (`ID3D11Device5::OpenSharedFence`) and the
    /// `ID3D11DeviceContext4` the GPU-side waits/signals need. `None` — logged — when the device
    /// predates D3D11.4 or either open fails; the ring then runs on the keyed-mutex arm.
    fn open_fences(
        ep: &RingEndpoint,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Option<Fences> {
        let (Ok(dev5), Ok(ctx4)) = (
            device.cast::<ID3D11Device5>(),
            context.cast::<ID3D11DeviceContext4>(),
        ) else {
            dbglog!(
                "[pf-vd] frame-push(driver): no ID3D11Device5/DeviceContext4 — fence ring unavailable on this device"
            );
            return None;
        };
        let open = |h: u64| -> windows::core::Result<ID3D11Fence> {
            let mut f: Option<ID3D11Fence> = None;
            // SAFETY: `dev5` is a live ID3D11Device5; `h` is the duplicated shared-fence NT handle,
            // alive for the endpoint's lifetime; `f` is a valid out-param checked below.
            unsafe { dev5.OpenSharedFence(HANDLE(h as usize as *mut core::ffi::c_void), &mut f)? };
            f.ok_or_else(|| windows::core::HRESULT(0x8000_4005u32 as i32).into())
        };
        match (open(ep.ready_fence), open(ep.retire_fence)) {
            (Ok(ready), Ok(retire)) => Some(Fences {
                ctx4,
                ready,
                retire,
            }),
            (r, t) => {
                let code = r.err().or(t.err()).map_or(0, |e| e.code().0);
                dbglog!(
                    "[pf-vd] frame-push(driver): OpenSharedFence failed rc={code:#x} — fence ring unavailable on this device"
                );
                None
            }
        }
    }

    /// The endpoint this publisher writes into.
    pub fn endpoint(&self) -> &Arc<RingEndpoint> {
        &self.ep
    }

    /// See [`RingEndpoint::is_stale`].
    pub fn is_stale(&self) -> bool {
        self.ep.is_stale()
    }

    /// v2 telemetry tail, drain side (stall attribution): stamp the heartbeat on EVERY drain-loop
    /// pass — and the last-acquire on a pass that actually acquired a composed frame — so the host
    /// can split a capture stall into "our worker starved" (heartbeat went stale) vs "the worker
    /// drained E_PENDING the whole hole — DWM composed nothing" (heartbeat fresh, last-acquire
    /// stale). Gated on the host's stamped header version (see the `telemetry` field docs);
    /// best-effort Relaxed stores, the `driver_status` visibility contract.
    pub fn note_drain(&self, acquired: bool) {
        if !self.ep.telemetry {
            return;
        }
        let qpc = qpc_now();
        if qpc == 0 {
            return;
        }
        let header = self.ep.header;
        // SAFETY: the header stays mapped for the endpoint's lifetime and the version gate above
        // proves the host built the v2 (88-byte) layout; both fields are naturally-aligned u64s
        // within it, valid for `AtomicU64` views (the same pattern as `latest_cell`).
        unsafe {
            (*(core::ptr::addr_of!((*header).drain_heartbeat_qpc) as *const AtomicU64))
                .store(qpc, Ordering::Relaxed);
            if acquired {
                (*(core::ptr::addr_of!((*header).last_acquire_qpc) as *const AtomicU64))
                    .store(qpc, Ordering::Relaxed);
            }
        }
    }

    /// Mirror the live diagnostic counters into the header's detail word (proto
    /// `pack_opened_detail`) — read by the host's first-frame timeout to name a no-frames failure —
    /// plus, on a telemetry-capable (v2) header, the full-width `offered_total` the host deltas
    /// across a stall window (the packed 15-bit counter saturates, so it can't be delta'd).
    #[inline]
    fn write_opened_detail(&self) {
        let header = self.ep.header;
        // SAFETY: the header stays mapped for the endpoint's lifetime; `driver_status_detail` is a
        // plain in-bounds u32 field — a best-effort diagnostic write.
        unsafe {
            (*header).driver_status_detail = pack_opened_detail(self.offered, self.mismatch_drops);
        }
        if self.ep.telemetry {
            // SAFETY: the version gate proves the host built the v2 (88-byte) layout;
            // `offered_total` is a naturally-aligned u64 within it (the `latest_cell` pattern).
            unsafe {
                (*(core::ptr::addr_of!((*header).offered_total) as *const AtomicU64))
                    .store(self.offered_total, Ordering::Relaxed);
            }
        }
    }

    /// Copy the most recently PUBLISHED frame out of the ring into `stash` — called just before this
    /// publisher is dropped for a supersede (a mid-session ring recreate, or a new session's channel
    /// delivery), when the ring it wrote is about to become unreachable. Between sessions the driver
    /// keeps publishing into the previous (host-side dead) ring, so the last-written slot IS the
    /// current desktop image — harvesting it seeds the next attach's first-frame republish. Skips
    /// when the stash already holds fresher content (see [`FrameStash::stored_at`]) or the slot's
    /// keyed mutex can't be had within 8 ms (a live host mid-consume — frames are flowing anyway).
    pub fn harvest_into(&self, device: &ID3D11Device, stash: &mut FrameStash) {
        let Some((slot, at)) = self.last_published else {
            return;
        };
        if stash.stored_at().is_some_and(|s| s >= at) {
            return;
        }
        let Some(s) = self.slots.get(slot as usize) else {
            return;
        };
        if self.fences.is_some() {
            // Fence protocol: own the slot for the copy by CASing it out of PUBLISHED (a slot the
            // host is READING, or already freed, is left alone — the stash keeps what it had). Our
            // copy is ordered after our own publish copy on this context, and the state goes back
            // to PUBLISHED so the host may still consume it.
            let state = self.ep.slot_state(slot as usize);
            if state
                .compare_exchange(
                    fence::PUBLISHED,
                    fence::WRITING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                stash.store(device, &self.context, &s.tex, at);
                state.store(fence::PUBLISHED, Ordering::Release);
            }
            return;
        }
        let Some(mutex) = s.mutex.as_ref() else {
            return;
        };
        // SAFETY: `mutex` is the live keyed mutex on this ring slot's shared texture; an 8 ms
        // try-acquire of key 0. Raw vtable call for the same reason as `publish` below: the `Result`
        // wrapper erases the success-severity WAIT_TIMEOUT/WAIT_ABANDONED codes.
        let hr = unsafe { (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), 0, 8) };
        match hr.0 {
            // Acquired cleanly — harvest the last-published image.
            0 => {
                // STRAIGHT-LINE between acquire and release (`store` is infallible-by-contract:
                // best-effort, no early return propagates past it), so the lock cannot leak.
                stash.store(device, &self.context, &s.tex, at);
                // SAFETY: the keyed mutex is held (acquired above); release it exactly once.
                unsafe {
                    let _ = mutex.ReleaseSync(0);
                }
            }
            // WAIT_ABANDONED: the host died holding the slot, so the surface's consistency is
            // unknown — take NO pixel action (a torn image must not become the next session's
            // first frame). Only the cleanup ownership the API handed over is discharged.
            WAIT_ABANDONED_HRESULT => {
                // SAFETY: abandoned still transfers the lock; release it exactly once.
                unsafe {
                    let _ = mutex.ReleaseSync(0);
                }
            }
            // Busy or a genuine error — keep whatever the stash had.
            _ => {}
        }
    }

    /// Copy `surface` into the next free ring slot and signal the host. Never blocks (0 ms try-acquire).
    ///
    /// `display_qpc` is the OS's `PresentDisplayQPCTime` for this frame (0 = none reported, or a
    /// stash republish) — stamped into the header's `qpc_pts` as the host's source-provenance
    /// clock.
    pub fn publish(&mut self, surface: &ID3D11Texture2D, display_qpc: u64) -> PublishOutcome {
        let ring_len = self.slots.len() as u32;
        if ring_len == 0 {
            return PublishOutcome::Dropped;
        }
        // Format guard: `CopyResource` needs the surface + ring textures to share a DXGI format. Drop a
        // frame that doesn't match (e.g. an FP16 HDR surface arriving while the ring is still BGRA, before
        // the host recreates the ring as FP16) instead of corrupting / failing the copy.
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `surface` is a live ID3D11Texture2D (borrowed from IddCx); `desc` is a valid local out-param.
        unsafe { surface.GetDesc(&mut desc) };
        // Descriptor guard: CopyResource needs the surface + ring textures to share format AND dimensions.
        // A fullscreen game can mode-set the display, changing the surface's format/size before the host
        // recreates the ring to match (game-capture bug GB1) — drop a mismatched frame (else garbage) and
        // report the ACTUAL descriptor once per episode so a repro shows exactly what changed.
        // SAFETY: the header stays mapped for the endpoint's lifetime; width/height are plain u32 fields.
        let (rw, rh) = unsafe { ((*self.ep.header).width, (*self.ep.header).height) };
        // Live diagnostics: count every surface offered (and, below, every mismatch drop) into the
        // header's detail word — what lets the host's first-frame timeout tell "DWM never composed"
        // from "every compose mismatched the ring". Written once per call, after the outcome is known.
        self.offered = self.offered.saturating_add(1);
        self.offered_total = self.offered_total.wrapping_add(1);
        if desc.Format.0 as u32 != self.ring_format || desc.Width != rw || desc.Height != rh {
            self.mismatch_drops = self.mismatch_drops.saturating_add(1);
            self.write_opened_detail();
            if !self.mismatch_logged {
                self.mismatch_logged = true;
                dbglog!(
                    "[pf-vd] frame-push DROP: surface {}x{} fmt={} != ring {}x{} fmt={} — display mode-set? (host should recreate the ring)",
                    desc.Width,
                    desc.Height,
                    desc.Format.0 as u32,
                    rw,
                    rh,
                    self.ring_format
                );
            }
            self.ep.note_drop();
            return PublishOutcome::DescMismatch;
        }
        self.mismatch_logged = false;
        self.write_opened_detail();
        if self.fences.is_some() {
            return self.publish_fence(surface, display_qpc);
        }
        let start = self.next;
        for attempt in 0..ring_len {
            let slot = (start + attempt) % ring_len;
            let s = &self.slots[slot as usize];
            let Some(mutex) = s.mutex.as_ref() else {
                continue; // cannot happen on the mutex arm (open refuses such a ring)
            };
            // SAFETY: `mutex` is the live keyed mutex on this ring slot's shared texture; a 0 ms
            // try-acquire of key 0 (released below; on WAIT_TIMEOUT it's never held). Raw vtable
            // call, NOT the `Result` wrapper: `.ok()` erases success codes, so through `Result` a
            // WAIT_TIMEOUT (host holds the slot) is indistinguishable from a real acquire — the
            // wrapper made the busy-skip arm below dead code and had us copying into (and
            // publishing) a slot the host was still reading.
            let hr =
                unsafe { (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), 0, 0) };
            match hr.0 {
                // WAIT_ABANDONED: the host died (or a host thread crashed) holding the slot — the
                // surface's consistency is unknown and this generation is DEAD. No pixel action;
                // discharge the cleanup ownership the API handed over (a failed cleanup release
                // does not delay the poisoning — the outcome is fatal either way) and tell the
                // caller to stop using this publisher.
                WAIT_ABANDONED_HRESULT => {
                    // SAFETY: abandoned still transfers the lock; best-effort release, once.
                    unsafe {
                        let _ = mutex.ReleaseSync(0);
                    }
                    dbglog!(
                        "[pf-vd] frame-push FATAL: slot {slot} keyed mutex ABANDONED (host died holding it) — poisoning this ring generation"
                    );
                    self.ep.note_drop();
                    self.ep
                        .mark_dead(ERR_DOMAIN_TRANSPORT, WAIT_ABANDONED_HRESULT);
                    return PublishOutcome::HostAbandoned;
                }
                // Acquired cleanly.
                0 => {
                    // STRAIGHT-LINE, NO `?` between acquire + release — a `?`-return would leak
                    // the keyed-mutex lock and wedge the host on this slot. Ordering is
                    // load-bearing: the copy is GPU-ordered via the mutex, and `latest` stores
                    // only after a CHECKED release — a slot whose release failed can never be
                    // re-acquired by the host and must not be published.

                    // SAFETY: `s.tex`/`surface` are live, format-matched (checked above) D3D
                    // textures on `self.context`'s device; the mutex is held, released once.
                    let released = unsafe {
                        self.context.CopyResource(&s.tex, surface);
                        mutex.ReleaseSync(0)
                    };
                    if let Err(e) = released {
                        dbglog!(
                            "[pf-vd] frame-push FATAL: slot {slot} ReleaseSync failed rc={:#x} — NOT publishing `latest`; poisoning this ring generation",
                            e.code().0
                        );
                        self.ep.note_drop();
                        self.ep.mark_dead(ERR_DOMAIN_TRANSPORT, e.code().0);
                        return PublishOutcome::Fatal;
                    }
                    let seq = self.ep.seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                    return self.published(slot, seq, display_qpc);
                }
                // Busy — the host holds this slot (the designed backpressure): try the next one.
                WAIT_TIMEOUT_HRESULT => continue,
                // Genuine failure (negative HRESULT — device removed / invalid call): this ring
                // generation is done. Name the device-removed reason while it is still queryable.
                _ => {
                    // SAFETY: `self.context` is the publisher's live immediate context;
                    // `GetDevice`/`GetDeviceRemovedReason` only read it.
                    let removed = unsafe {
                        self.context.GetDevice().map_or(0, |d| {
                            d.GetDeviceRemovedReason()
                                .map_or_else(|e| e.code().0, |()| 0)
                        })
                    };
                    dbglog!(
                        "[pf-vd] frame-push FATAL: slot {slot} AcquireSync rc={:#x} (device-removed reason {removed:#x}) — poisoning this ring generation",
                        hr.0
                    );
                    self.ep.note_drop();
                    self.ep.mark_dead(
                        if removed != 0 {
                            pf_driver_proto::frame::ERR_DOMAIN_DEVICE
                        } else {
                            ERR_DOMAIN_TRANSPORT
                        },
                        if removed != 0 { removed } else { hr.0 },
                    );
                    return PublishOutcome::Fatal;
                }
            }
        }
        // All slots busy — the designed backpressure (never block the swap-chain thread). Distinct
        // from the fatal outcomes: the host is alive and behind, retrying next compose is correct.
        self.ep.note_drop();
        PublishOutcome::AllSlotsBusy
    }

    /// The fence-protocol publish (immunity plan D5; the S2-proven shape). One scan, one CAS
    /// claim, GPU-ordered copy — the producer never CPU-waits on the reader: it takes a FREE
    /// slot, else overwrites the OLDEST PUBLISHED one, else drops the frame; READING and WRITING
    /// slots are never touched.
    fn publish_fence(&mut self, surface: &ID3D11Texture2D, display_qpc: u64) -> PublishOutcome {
        let ring_len = self.slots.len();
        let ep = self.ep.clone();
        // A superseded generation must not touch the slot table the host is rebuilding — the
        // worker retires this publisher on its next pass; this closes the last publish before it.
        if ep.is_stale() {
            return PublishOutcome::Dropped;
        }
        let mut view = [(fence::FREE, 0u64); RING_LEN as usize];
        for (i, v) in view.iter_mut().enumerate().take(ring_len) {
            *v = (
                ep.slot_state(i).load(Ordering::Acquire),
                ep.slot_u64(i, offset_of!(SlotRecord, seq))
                    .load(Ordering::Acquire),
            );
        }
        let (slot, from) = match fence::producer_claim(&view[..ring_len]) {
            Claim::Free(i) => (i, fence::FREE),
            Claim::Overwrite(i) => (i, fence::PUBLISHED),
            Claim::Drop => {
                ep.note_drop();
                return PublishOutcome::AllSlotsBusy;
            }
        };
        // The claim CAS: a lost race (the host took the slot READING, or freed it) is the
        // designed race — drop this frame and rescan on the next compose rather than spin.
        if ep
            .slot_state(slot)
            .compare_exchange(from, fence::WRITING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            ep.note_drop();
            return PublishOutcome::AllSlotsBusy;
        }
        let Some(f) = self.fences.as_ref() else {
            return PublishOutcome::Dropped;
        };
        let retire_v = ep
            .slot_u64(slot, offset_of!(SlotRecord, retire_value))
            .load(Ordering::Acquire);
        let ready_v = ep
            .ready_value
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        // SAFETY: GPU-queued calls over live COM objects on this worker's immediate context: the
        // retire Wait orders our copy after the host's last read of this slot on the GPU timeline
        // (never a CPU block), the copy is format-matched (checked by the caller), and the ready
        // Signal orders it before the host's consume.
        let queued = unsafe {
            f.ctx4.Wait(&f.retire, retire_v).and_then(|()| {
                self.context.CopyResource(&self.slots[slot].tex, surface);
                f.ctx4.Signal(&f.ready, ready_v)
            })
        };
        if let Err(e) = queued {
            // Give the slot back (its pixels are unknown, so FREE — the consumer never reads a FREE
            // slot) and poison the generation: a failed fence op is a device-class fatal.
            ep.slot_state(slot).store(fence::FREE, Ordering::Release);
            // SAFETY: `self.context` is the live immediate context; a read-only status query.
            let removed = unsafe {
                self.context.GetDevice().map_or(0, |d| {
                    d.GetDeviceRemovedReason()
                        .map_or_else(|e| e.code().0, |()| 0)
                })
            };
            dbglog!(
                "[pf-vd] frame-push FATAL: fence Wait/Signal failed rc={:#x} (device-removed reason {removed:#x}) — poisoning this ring generation",
                e.code().0
            );
            ep.note_drop();
            ep.mark_dead(
                if removed != 0 {
                    ERR_DOMAIN_DEVICE
                } else {
                    ERR_DOMAIN_TRANSPORT
                },
                if removed != 0 { removed } else { e.code().0 },
            );
            return PublishOutcome::Fatal;
        }
        let seq = ep.seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        // Stamp the record, then release PUBLISHED: a consumer that Acquire-loads PUBLISHED sees
        // the seq and ready value that belong to these pixels. The record carries the PACKED
        // token (generation included), so a record a superseded generation left behind is
        // recognisable to the host and freed, never consumed by sequence number.
        let record_seq = FrameToken {
            generation: ep.generation,
            seq: seq as u32,
            slot: slot as u8,
        }
        .pack();
        ep.slot_u64(slot, offset_of!(SlotRecord, seq))
            .store(record_seq, Ordering::Release);
        ep.slot_u64(slot, offset_of!(SlotRecord, ready_value))
            .store(ready_v, Ordering::Release);
        ep.slot_state(slot)
            .store(fence::PUBLISHED, Ordering::Release);
        self.published(slot as u32, seq, display_qpc)
    }

    /// The publish tail both arms share once a slot's pixels are in place: the token, the
    /// provenance stamp, the v3 counters and the host wake-up.
    fn published(&mut self, slot: u32, seq: u64, display_qpc: u64) -> PublishOutcome {
        let ep = &*self.ep;
        // `latest` = (generation << 40) | (seq << 8) | slot, packed by the proto's `FrameToken`
        // (single source of truth — the host unpacks with the same type). Stamping the generation
        // lets the host REJECT a publish from a stale ring (an old-generation publisher racing the
        // host's mid-session ring recreate) so it never consumes an unwritten new-ring slot.
        let latest = FrameToken {
            generation: ep.generation,
            seq: seq as u32,
            slot: slot as u8,
        }
        .pack();
        // Provenance stamp BEFORE the Release publish of `latest`: a host that reads it after
        // loading the token sees this frame's stamp or a newer one — monotonic either way, and
        // best-effort like the telemetry tail.
        // SAFETY: the header stays mapped for the endpoint's lifetime; `qpc_pts` is an 8-aligned
        // u64 within it (the `latest_cell` pattern).
        unsafe {
            (*(core::ptr::addr_of!((*ep.header).qpc_pts) as *const AtomicU64))
                .store(display_qpc, Ordering::Relaxed);
        }
        ep.latest_cell().store(latest, Ordering::Release);
        // v3 tail: a NEW source frame (the OS stamped a present time) advances the MONITOR's
        // source sequence; a stash republish (qpc 0) does not. Counters + publish QPC are
        // best-effort Relaxed like the telemetry tail.
        if display_qpc != 0 {
            let n = ep
                .source_seq
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1);
            ep.v3_store_u64(offset_of!(SharedHeader, source_sequence), n);
        }
        let n = ep
            .published_total
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        ep.v3_store_u64(offset_of!(SharedHeader, published_total), n);
        ep.v3_store_u64(offset_of!(SharedHeader, last_publish_qpc), qpc_now());
        // SAFETY: `ep.event` is the live host-created frame-ready event, duplicated into this
        // process with the creator's access; signalling it wakes the host consumer.
        unsafe {
            let _ = SetEvent(ep.event);
        }
        self.next = (slot + 1) % self.slots.len() as u32;
        self.last_published = Some((slot, Instant::now()));
        PublishOutcome::Published
    }
}
