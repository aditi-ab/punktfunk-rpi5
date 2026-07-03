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

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use pf_driver_proto::control::SetFrameChannelRequest;
use pf_driver_proto::frame::{
    DRV_STATUS_NO_DEVICE1, DRV_STATUS_OPENED, DRV_STATUS_TEX_FAIL, FrameToken, MAGIC, RING_LEN,
    SharedHeader,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;
use windows::Win32::System::Memory::{
    FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows::Win32::System::Threading::SetEvent;
use windows::core::Interface;

/// `WAIT_TIMEOUT` as an HRESULT — `AcquireSync` returns this when the slot is held by the consumer.
const WAIT_TIMEOUT_HRESULT: i32 = 0x0000_0102;

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
}

// SAFETY: created and used only on the swap-chain processor thread.
unsafe impl Send for FramePublisher {}

impl FramePublisher {
    /// Attach to the host ring from a delivered [`FrameChannel`]. Consumes the channel: on ANY failure
    /// every handle is closed (taken ones explicitly, the rest by the channel's `Drop`) and the host
    /// re-delivers on the next recreate — there is nothing to poll, so failure is terminal for THIS
    /// delivery (the host's `wait_for_attach` sees the status code and fails the session open). All
    /// early-return paths clean up explicitly (raw-handle style, no RAII — matches the rest of this
    /// driver).
    pub fn from_channel(
        mut channel: FrameChannel,
        render_luid_low: u32,
        render_luid_high: i32,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> windows::core::Result<Self> {
        let ring_len = channel.ring_len;

        // 1. Map the header from the duplicated section handle (ours from here on).
        let map = FrameChannel::take(&mut channel.header);
        // SAFETY: `map` is the live section handle the host duplicated into this process; mapping
        // size_of::<SharedHeader>() bytes of it (the host created the mapping at >= that size). The null
        // `view.Value` is checked below.
        let view = unsafe {
            // Read/write only — the host now duplicates the header handle with least access
            // (`SECTION_MAP_READ | SECTION_MAP_WRITE`), so `FILE_MAP_ALL_ACCESS` would exceed the
            // granted rights and fail. We read the layout + write status/publish-token fields; RW covers it.
            MapViewOfFile(
                map,
                FILE_MAP_READ | FILE_MAP_WRITE,
                0,
                0,
                core::mem::size_of::<SharedHeader>(),
            )
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

        // 3. The host stamps magic==MAGIC BEFORE delivering the channel, and this channel's generation
        //    must match the header's CURRENT generation — a mismatch means the host recreated the ring
        //    again before we attached (a fresh delivery is on its way); drop this stale one.
        // SAFETY: `header` is the mapped host header; `magic`/`generation` live within it and are read
        // atomically (Acquire) to pair with the host's Release publishes.
        let (magic, header_gen) = unsafe {
            (
                (*(core::ptr::addr_of!((*header).magic) as *const AtomicU32))
                    .load(Ordering::Acquire),
                (*(core::ptr::addr_of!((*header).generation) as *const AtomicU32))
                    .load(Ordering::Acquire),
            )
        };
        if magic != MAGIC || header_gen != channel.generation {
            dbglog!(
                "[pf-vd] frame-push(driver): dropping channel delivery (magic ok: {}, channel gen {} vs header gen {header_gen})",
                magic == MAGIC,
                channel.generation
            );
            // SAFETY: `header`/`map` are the live mapped view + taken handle; unmapped + closed once on
            // this path.
            unsafe {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: header.cast(),
                });
                let _ = CloseHandle(map);
            }
            // E_BOUNDS — stand-in for "stale delivery"; the caller only drops the attempt.
            return Err(windows::core::HRESULT(0x8000_000Bu32 as i32).into());
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

        // SAFETY: `header` is the mapped host header; the status field lives within it.
        unsafe {
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
        })
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

    /// Copy `surface` into the next free ring slot and signal the host. Never blocks (0 ms try-acquire).
    pub fn publish(&mut self, surface: &ID3D11Texture2D) {
        let ring_len = self.slots.len() as u32;
        if ring_len == 0 {
            return;
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
        if desc.Format.0 as u32 != self.ring_format || desc.Width != rw || desc.Height != rh {
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
            return;
        }
        self.mismatch_logged = false;
        let start = self.next;
        for attempt in 0..ring_len {
            let slot = (start + attempt) % ring_len;
            let s = &self.slots[slot as usize];
            // SAFETY: `s.mutex` is the live keyed mutex on this ring slot's shared texture; a 0 ms
            // try-acquire of key 0 (released below or on WAIT_TIMEOUT it's never held).
            match unsafe { s.mutex.AcquireSync(0, 0) } {
                Ok(()) => {
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
                    self.latest_cell().store(latest, Ordering::Release);
                    // SAFETY: `self.event` is the live host-created frame-ready event, duplicated into
                    // this process with the creator's access; signalling it wakes the host consumer.
                    unsafe {
                        let _ = SetEvent(self.event);
                    }
                    self.next = (slot + 1) % ring_len;
                    return;
                }
                Err(e) if e.code().0 == WAIT_TIMEOUT_HRESULT => continue,
                Err(_) => return,
            }
        }
        // All slots busy — drop this frame (never block the swap-chain thread).
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
