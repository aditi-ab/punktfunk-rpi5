//! STEP 6 — IDD-push frame publisher (DRIVER side), attached over the **sealed channel**.
//!
//! The restricted WUDFHost token canNOT create named kernel objects — and since the frame channel
//! carries whole-desktop pixels, the objects are not merely host-created but **unnamed**: nothing to
//! enumerate, open by name, or pre-create ("squat"). The **host** creates the shared header +
//! frame-ready event + ring of keyed-mutex textures with no names, duplicates the handles INTO this
//! WUDFHost process (`DuplicateHandle` — SYSTEM can, we can't reciprocate, which is why the host is the
//! broker), and delivers the handle VALUES over `IOCTL_SET_FRAME_CHANNEL` ([`crate::control`] stashes
//! them per monitor as a [`FrameChannel`]). The swap-chain worker picks the stash up and attaches with
//! [`FramePublisher::from_channel`]. Only the two endpoint processes ever hold a handle to any frame
//! object — see `design/idd-push-security.md`.
//!
//! The driver writes its actual render-adapter LUID + a status code back into the host-created header
//! (our only driver-visibility channel: UMDF hides OutputDebugString in ETW and the token can't write
//! files), then copies each acquired swap-chain surface into the next ring slot and signals the host.
//!
//! Host counterpart: `crates/punktfunk-host/src/capture/windows/idd_push.rs`. The shared `SharedHeader`
//! layout, the [`FrameToken`] packing, the `MAGIC`/`RING_LEN`, the `DRV_STATUS_*` codes and the
//! channel-delivery struct are NOT hand-duplicated here: both sides `use pf_driver_proto::{control,
//! frame}`, which OWNS the contract (with `const` size asserts so any drift is a compile error).
//!
//! This module also owns the [`FrameStash`] — the driver-retained copy of the most recent composed
//! frame that the swap-chain worker republishes into every freshly-attached ring, so a session
//! opening onto an IDLE desktop gets its first frame immediately instead of waiting for something
//! to dirty the display (the first-frame guarantee; see the type docs). Purely driver-internal
//! behavior — no proto change: the host just sees a normal `seq = 1` publish right after attach.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use pf_driver_proto::control::SetFrameChannelRequest;
use pf_driver_proto::frame::{
    AttachReject, DRV_STATUS_BIND_FAIL, DRV_STATUS_NO_DEVICE1, DRV_STATUS_OPENED,
    DRV_STATUS_TEX_FAIL, FrameToken, RING_LEN, SharedHeader, VERSION_TELEMETRY, check_attach,
    pack_opened_detail,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11Device1, ID3D11DeviceContext,
    ID3D11Texture2D,
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
    header: u64,
    event: u64,
    textures: [u64; RING_LEN as usize],
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
            header: req.header_handle,
            event: req.event_handle,
            textures: req.texture_handles,
        })
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
    }
}

impl Drop for FrameChannel {
    fn drop(&mut self) {
        for v in [&mut self.header, &mut self.event]
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

struct Slot {
    tex: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
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
    /// Dropped without publishing (all slots busy / device error) — the host is alive and
    /// consuming, so there is nothing to retain.
    Dropped,
}

/// Publishes acquired swap-chain surfaces into the HOST-created ring. Owned by the swap-chain processor
/// thread; attached lazily once the host's channel delivery lands in the monitor stash.
pub struct FramePublisher {
    context: ID3D11DeviceContext,
    map: HANDLE,
    header: *mut SharedHeader,
    event: HANDLE,
    slots: Vec<Slot>,
    next: u32,
    seq: u64,
    /// The host-created ring textures' DXGI format (from the shared header). A swap-chain surface whose
    /// format differs (e.g. an FP16 HDR frame vs a BGRA ring) is dropped in `publish` — `CopyResource`
    /// needs matching formats.
    ring_format: u32,
    /// The ring generation this publisher attached to. The host BUMPS the header generation when it
    /// recreates the ring at a new format mid-session (the display's HDR mode flipped) — [`Self::is_stale`]
    /// detects that so `run_core` re-attaches to the new ring (whose channel the host re-delivers)
    /// instead of dropping every frame.
    generation: u32,
    /// Set when a surface is dropped for a descriptor mismatch (a game mode-set the display), cleared on a
    /// matched publish — throttles the drop log to once per mismatch episode (game-capture bug GB1).
    mismatch_logged: bool,
    /// Live diagnostic counters mirrored into `SharedHeader::driver_status_detail` after every
    /// `publish()` (see proto `pack_opened_detail`): surfaces OFFERED to the ring, and how many of
    /// those were DROPPED for a descriptor mismatch. What lets the host's first-frame timeout tell
    /// "DWM never composed" from "every compose mismatched the ring".
    offered: u32,
    mismatch_drops: u32,
    /// Whether the HOST created a telemetry-capable (v2, 88-byte) header — stamped `version >=
    /// VERSION_TELEMETRY` at attach. Gates every write to the telemetry tail: a v1 host's header
    /// is 64 bytes, and the tail fields would land past the layout it reads.
    telemetry: bool,
    /// Full-width wrapping sibling of `offered` (which packs to 15 saturating bits) — mirrored
    /// into `SharedHeader::offered_total` so the host can delta it across a stall window.
    offered_total: u64,
    /// The slot of the most recent successful publish + when it happened — what [`Self::harvest_into`]
    /// reads when this publisher is superseded. `None` until the first publish.
    last_published: Option<(u32, Instant)>,
    /// The render adapter (LUID) this publisher's device + opened ring textures live on. A worker
    /// re-adopts a publisher preserved across a swap-chain unassign→reassign flap ONLY when the
    /// freshly-assigned swap-chain renders on this SAME adapter (else the opened textures would be
    /// cross-device); see [`Self::render_adapter`] + `swap_chain_processor::run_core`.
    render_luid_low: u32,
    render_luid_high: i32,
}

// SAFETY: created and used only on the swap-chain processor thread.
unsafe impl Send for FramePublisher {}

impl FramePublisher {
    /// Attach to the host ring from a delivered [`FrameChannel`]. Consumes the channel: on ANY failure
    /// every handle is closed (taken ones explicitly, the rest by the channel's `Drop`) and the host
    /// re-delivers on the next recreate — there is nothing to poll, so failure is terminal for THIS
    /// delivery (the host's `wait_for_attach` sees the status code and fails the session open). All
    /// early-return paths clean up explicitly (raw-handle style, no RAII — matches the rest of this
    /// driver). `target_id` is the OWNING monitor's OS target id: the mapped ring must name it
    /// (proto v3 binding validation — see step 3), so a cross-delivered ring can never carry this
    /// monitor's frames into another client's stream.
    pub fn from_channel(
        mut channel: FrameChannel,
        target_id: u32,
        render_luid_low: u32,
        render_luid_high: i32,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> windows::core::Result<Self> {
        let ring_len = channel.ring_len;

        // 1. Map the header from the duplicated section handle (ours from here on).
        let map = FrameChannel::take(&mut channel.header);
        // SAFETY: `map` is the live section handle the host duplicated into this process; a byte
        // count of 0 maps the WHOLE section — a v1 host created a 64-byte (pre-telemetry) header,
        // so requesting size_of::<SharedHeader>() (the v2 88 bytes) could exceed what that host
        // declared, while 0 always fits and always covers the layout the host actually built (the
        // `telemetry` version gate keeps our writes inside it). The null `view.Value` is checked below.
        let view = unsafe {
            // Read/write only — the host now duplicates the header handle with least access
            // (`SECTION_MAP_READ | SECTION_MAP_WRITE`), so `FILE_MAP_ALL_ACCESS` would exceed the
            // granted rights and fail. We read the layout + write status/publish-token fields; RW covers it.
            MapViewOfFile(map, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0)
        };
        if view.Value.is_null() {
            let err = windows::core::Error::from_win32();
            // SAFETY: `map` is the taken section handle, closed once here on the error path (the rest of
            // `channel` closes via its Drop).
            unsafe {
                let _ = CloseHandle(map);
            }
            return Err(err);
        }
        let header = view.Value.cast::<SharedHeader>();

        // 2. Report our render adapter to the host immediately (lets it detect a mismatch).
        // SAFETY: `header` points to the mapped, non-null host header (>= size_of::<SharedHeader>()
        // bytes); these scalar writes are within it.
        unsafe {
            (*header).driver_render_luid_low = render_luid_low;
            (*header).driver_render_luid_high = render_luid_high;
        }

        // 3. The host stamps magic==MAGIC BEFORE delivering the channel, this channel's generation
        //    must match the header's CURRENT generation (a mismatch means the host recreated the ring
        //    again before we attached — a fresh delivery is on its way; drop this stale one), and —
        //    proto v3, `design/idd-push-security.md` invariant #10 — the mapped ring must NAME THIS
        //    MONITOR: the host stamps `target_id` before the magic, so with parallel displays a
        //    host-side stash cross-wire fails CLOSED here instead of publishing this monitor's frames
        //    into another client's ring. The shared `check_attach` (unit-tested in pf-driver-proto)
        //    owns the precedence: staleness first, binding second.
        // SAFETY: `header` is the mapped host header; `magic`/`generation` live within it and are read
        // atomically (Acquire) to pair with the host's Release publishes; `target_id`/`version` are
        // plain in-bounds u32 reads, stamped before the magic the Acquire load ordered us behind.
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
                // SAFETY: `header`/`map` are the live mapped view + taken handle; unmapped + closed once
                // on this path.
                unsafe {
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: header.cast(),
                    });
                    let _ = CloseHandle(map);
                }
                // E_BOUNDS — stand-in for "stale delivery"; the caller only drops the attempt.
                return Err(windows::core::HRESULT(0x8000_000Bu32 as i32).into());
            }
            Err(AttachReject::BindMismatch) => {
                dbglog!(
                    "[pf-vd] frame-push(driver): REFUSING attach — ring names target {header_target}, this monitor is {target_id} (host stash cross-wire?)"
                );
                // Report the refusal through the header so the host's wait_for_attach fails the open
                // LOUDLY (DRV_STATUS_BIND_FAIL) instead of timing out mute; the detail carries the
                // target id the ring claims.
                // SAFETY: `header`/`map` are the live mapped view + taken handle; the status writes are
                // in-bounds scalar writes, then both are released exactly once on this path.
                unsafe {
                    (*header).driver_status_detail = header_target;
                    (*header).driver_status = DRV_STATUS_BIND_FAIL;
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: header.cast(),
                    });
                    let _ = CloseHandle(map);
                }
                // E_INVALIDARG — the delivery itself is wrong; the caller only drops the attempt.
                return Err(windows::core::HRESULT(0x8007_0057u32 as i32).into());
            }
        }

        // 4. The frame-ready event (duplicated with the host handle's full access, so SetEvent works).
        let event = FrameChannel::take(&mut channel.event);

        // 5. Open device1 + the ring textures from their duplicated shared handles (same render adapter
        //    required). Each NT handle is closed right after the open — the COM object holds its own
        //    reference, and the HOST keeps the resource alive with its own handle.
        let device1: ID3D11Device1 = match device.cast() {
            Ok(d) => d,
            Err(e) => {
                // SAFETY: `header` is the mapped host header (status write within it); `event`/`map` are
                // the taken live handles, all released once on this error path.
                unsafe {
                    (*header).driver_status = DRV_STATUS_NO_DEVICE1;
                    let _ = CloseHandle(event);
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: header.cast(),
                    });
                    let _ = CloseHandle(map);
                }
                return Err(e);
            }
        };
        let mut slots = Vec::new();
        // Take each texture handle one at a time (NOT the whole array up front), so an error return
        // mid-loop still lets `channel`'s Drop close every not-yet-taken handle.
        for value in channel.textures.iter_mut().take(ring_len as usize) {
            let tex_handle = FrameChannel::take(value);
            // SAFETY: `device1` is a live ID3D11Device1; `tex_handle` is the duplicated shared NT handle
            // for this ring texture.
            let opened: windows::core::Result<ID3D11Texture2D> =
                unsafe { device1.OpenSharedResource1(tex_handle) };
            // SAFETY: `tex_handle` is ours (taken above) and no longer needed whether the open succeeded
            // (the COM object holds the resource) or failed — close it exactly once here.
            unsafe {
                let _ = CloseHandle(tex_handle);
            }
            let failed = match opened {
                Ok(tex) => match tex.cast::<IDXGIKeyedMutex>() {
                    Ok(mutex) => {
                        slots.push(Slot { tex, mutex });
                        None
                    }
                    Err(e) => Some(e),
                },
                // Most likely a render-adapter mismatch (the host made the textures on a different GPU
                // than the swap-chain renders on). Tell the host so it can report it.
                Err(e) => Some(e),
            };
            if let Some(e) = failed {
                // SAFETY: `header` is the mapped host header (status writes within it); `event`/`map`
                // are the taken live handles, all released once on this error path (the not-yet-taken
                // texture handles close via `channel`'s Drop).
                unsafe {
                    (*header).driver_status = DRV_STATUS_TEX_FAIL;
                    (*header).driver_status_detail = e.code().0 as u32;
                    let _ = CloseHandle(event);
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: header.cast(),
                    });
                    let _ = CloseHandle(map);
                }
                return Err(e);
            }
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
        dbglog!(
            "[pf-vd] frame-push(driver): attached to host ring gen {header_gen} ({ring_len} slots, sealed channel)"
        );
        Ok(Self {
            context: context.clone(),
            map,
            header,
            event,
            slots,
            next: 0,
            seq: 0,
            // SAFETY: `header` is the mapped host header; `dxgi_format` lives within it.
            ring_format: unsafe { (*header).dxgi_format },
            generation: header_gen,
            mismatch_logged: false,
            offered: 0,
            mismatch_drops: 0,
            telemetry: header_version >= VERSION_TELEMETRY,
            offered_total: 0,
            last_published: None,
            render_luid_low,
            render_luid_high,
        })
    }

    /// v2 telemetry tail, drain side (stall attribution): stamp the heartbeat on EVERY drain-loop
    /// pass — and the last-acquire on a pass that actually acquired a composed frame — so the host
    /// can split a capture stall into "our worker starved" (heartbeat went stale) vs "the worker
    /// drained E_PENDING the whole hole — DWM composed nothing" (heartbeat fresh, last-acquire
    /// stale). Gated on the host's stamped header version (see the `telemetry` field docs);
    /// best-effort Relaxed stores, the `driver_status` visibility contract.
    pub fn note_drain(&self, acquired: bool) {
        if !self.telemetry {
            return;
        }
        let mut qpc = 0i64;
        // SAFETY: plain FFI; `qpc` is a valid local out-param. QPC cannot fail on any OS we load on.
        if unsafe { QueryPerformanceCounter(&mut qpc) }.is_err() {
            return;
        }
        // SAFETY: `self.header` stays mapped for the publisher's lifetime and the version gate above
        // proves the host built the v2 (88-byte) layout; both fields are naturally-aligned u64s
        // within it, valid for `AtomicU64` views (the same pattern as `latest_cell`).
        unsafe {
            (*(core::ptr::addr_of!((*self.header).drain_heartbeat_qpc) as *const AtomicU64))
                .store(qpc as u64, Ordering::Relaxed);
            if acquired {
                (*(core::ptr::addr_of!((*self.header).last_acquire_qpc) as *const AtomicU64))
                    .store(qpc as u64, Ordering::Relaxed);
            }
        }
    }

    /// Mirror the live diagnostic counters into the header's detail word (proto
    /// `pack_opened_detail`) — read by the host's first-frame timeout to name a no-frames failure —
    /// plus, on a telemetry-capable (v2) header, the full-width `offered_total` the host deltas
    /// across a stall window (the packed 15-bit counter saturates, so it can't be delta'd).
    #[inline]
    fn write_opened_detail(&self) {
        // SAFETY: `self.header` stays mapped for the publisher's lifetime (unmapped only in Drop);
        // `driver_status_detail` is a plain in-bounds u32 field — a best-effort diagnostic write.
        unsafe {
            (*self.header).driver_status_detail =
                pack_opened_detail(self.offered, self.mismatch_drops);
        }
        if self.telemetry {
            // SAFETY: the version gate proves the host built the v2 (88-byte) layout;
            // `offered_total` is a naturally-aligned u64 within it (the `latest_cell` pattern).
            unsafe {
                (*(core::ptr::addr_of!((*self.header).offered_total) as *const AtomicU64))
                    .store(self.offered_total, Ordering::Relaxed);
            }
        }
    }

    #[inline]
    fn latest_cell(&self) -> &AtomicU64 {
        // SAFETY: `self.header` stays mapped for the publisher's lifetime (unmapped only in Drop); the
        // `latest` field lives within it and is naturally aligned, so this AtomicU64 reference is valid.
        unsafe { &*(core::ptr::addr_of!((*self.header).latest) as *const AtomicU64) }
    }

    /// True once the host has recreated the ring (bumped the header generation) — e.g. the display's HDR
    /// mode flipped, so the ring format changed (FP16 ⇄ BGRA) and a fresh channel delivery is coming.
    /// `run_core` drops the publisher on this so it re-attaches to the new ring.
    pub fn is_stale(&self) -> bool {
        // SAFETY: `self.header` stays mapped for the publisher's lifetime; `generation` lives within it and
        // is read atomically (Acquire) to pair with the host's Release bump on a mid-session ring recreate.
        let cur = unsafe {
            (*(core::ptr::addr_of!((*self.header).generation) as *const AtomicU32))
                .load(Ordering::Acquire)
        };
        cur != self.generation
    }

    /// The render adapter (LUID) this publisher's device + opened ring textures live on. The swap-chain
    /// worker re-adopts a publisher preserved across an unassign→reassign flap only when the freshly-
    /// assigned swap-chain renders on this same adapter (see the field docs + `run_core`).
    pub fn render_adapter(&self) -> (u32, i32) {
        (self.render_luid_low, self.render_luid_high)
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
        // SAFETY: `s.mutex` is the live keyed mutex on this ring slot's shared texture; an 8 ms
        // try-acquire of key 0. Raw vtable call for the same reason as `publish` below: the `Result`
        // wrapper erases the success-severity WAIT_TIMEOUT/WAIT_ABANDONED codes.
        let hr =
            unsafe { (Interface::vtable(&s.mutex).AcquireSync)(Interface::as_raw(&s.mutex), 0, 8) };
        match hr.0 {
            // Acquired — S_OK, or WAIT_ABANDONED (the host died holding the slot: ownership
            // transferred to us; exactly the between-sessions case the harvest exists for).
            0 | WAIT_ABANDONED_HRESULT => {
                // STRAIGHT-LINE between acquire and release (`store` is infallible-by-contract:
                // best-effort, no early return propagates past it), so the lock cannot leak.
                stash.store(device, &self.context, &s.tex, at);
                // SAFETY: the keyed mutex is held (acquired above); release it exactly once.
                unsafe {
                    let _ = s.mutex.ReleaseSync(0);
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
        // SAFETY: `self.header` stays mapped for the publisher's lifetime; width/height are plain u32 fields.
        let (rw, rh) = unsafe { ((*self.header).width, (*self.header).height) };
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
            return PublishOutcome::DescMismatch;
        }
        self.mismatch_logged = false;
        self.write_opened_detail();
        let start = self.next;
        for attempt in 0..ring_len {
            let slot = (start + attempt) % ring_len;
            let s = &self.slots[slot as usize];
            // SAFETY: `s.mutex` is the live keyed mutex on this ring slot's shared texture; a 0 ms
            // try-acquire of key 0 (released below; on WAIT_TIMEOUT it's never held). Raw vtable
            // call, NOT the `Result` wrapper: `.ok()` erases success codes, so through `Result` a
            // WAIT_TIMEOUT (host holds the slot) is indistinguishable from a real acquire — the
            // wrapper made the busy-skip arm below dead code and had us copying into (and
            // publishing) a slot the host was still reading.
            let hr = unsafe {
                (Interface::vtable(&s.mutex).AcquireSync)(Interface::as_raw(&s.mutex), 0, 0)
            };
            match hr.0 {
                // Acquired — S_OK, or WAIT_ABANDONED (the host died holding the slot: ownership
                // still transferred; publish normally, a dead host consumes nothing either way).
                0 | WAIT_ABANDONED_HRESULT => {
                    // STRAIGHT-LINE, NO `?` between acquire + release — a `?`-return here would leak the
                    // keyed-mutex lock and wedge the host on this slot. The ordering below is load-bearing:
                    // the CopyResource is GPU-ordered before the consumer via the slot keyed mutex, and the
                    // `latest` store (Release) publishes the slot only AFTER the copy is queued + the mutex
                    // released.
                    // SAFETY: `s.tex`/`surface` are live, format-matched (checked above) D3D textures on
                    // `self.context`'s device; the keyed mutex is held here, so we release it exactly once.
                    unsafe {
                        self.context.CopyResource(&s.tex, surface);
                        let _ = s.mutex.ReleaseSync(0);
                    }
                    self.seq = self.seq.wrapping_add(1);
                    // `latest` = (generation << 40) | (seq << 8) | slot, packed by the proto's `FrameToken`
                    // (single source of truth — the host unpacks with the same type). Stamping the generation
                    // lets the host REJECT a publish from a stale ring (an old-generation publisher racing the
                    // host's mid-session ring recreate) so it never consumes an unwritten new-ring slot.
                    let latest = FrameToken {
                        generation: self.generation,
                        seq: self.seq as u32,
                        slot: slot as u8,
                    }
                    .pack();
                    // Provenance stamp BEFORE the Release publish of `latest`: a host that reads
                    // it after loading the token sees this frame's stamp or a newer one —
                    // monotonic either way, and best-effort like the telemetry tail.
                    // SAFETY: `self.header` stays mapped for the publisher's lifetime; `qpc_pts`
                    // is an 8-aligned u64 within it (the `latest_cell` pattern).
                    unsafe {
                        (*(core::ptr::addr_of!((*self.header).qpc_pts) as *const AtomicU64))
                            .store(display_qpc, Ordering::Relaxed);
                    }
                    self.latest_cell().store(latest, Ordering::Release);
                    // SAFETY: `self.event` is the live host-created frame-ready event, duplicated into
                    // this process with the creator's access; signalling it wakes the host consumer.
                    unsafe {
                        let _ = SetEvent(self.event);
                    }
                    self.next = (slot + 1) % ring_len;
                    self.last_published = Some((slot, Instant::now()));
                    return PublishOutcome::Published;
                }
                // Busy — the host holds this slot (the designed backpressure): try the next one.
                WAIT_TIMEOUT_HRESULT => continue,
                // Genuine failure (negative HRESULT — device removed / invalid call): drop the frame.
                _ => return PublishOutcome::Dropped,
            }
        }
        // All slots busy — drop this frame (never block the swap-chain thread).
        PublishOutcome::Dropped
    }
}

impl Drop for FramePublisher {
    fn drop(&mut self) {
        // Slots FIRST (release the shared textures + keyed mutexes), THEN unmap the header, THEN the
        // handles — nothing of the channel outlives the publisher (teardown invariant,
        // `design/idd-push-security.md`).
        self.slots.clear();
        // SAFETY: drop runs once; `self.header` (if non-null) is the live mapped view and `self.event`/
        // `self.map` are the live handles this publisher owns — each unmapped/closed exactly once here.
        unsafe {
            if !self.header.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.header.cast(),
                });
            }
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.map);
        }
    }
}
