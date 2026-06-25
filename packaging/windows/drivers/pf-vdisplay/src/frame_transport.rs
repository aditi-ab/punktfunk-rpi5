//! STEP 6 — IDD-push frame publisher (DRIVER side).
//!
//! The restricted WUDFHost token canNOT create named kernel objects (proven on the RTX box: it can't
//! even write a world-writable file), so — exactly like the gamepad UMDF drivers
//! (`crates/punktfunk-host/src/inject/dualsense_windows.rs`: *"the host creates the section, privileged,
//! with a permissive SDDL so the WUDFHost can open it; the driver maps it"*) — the **host** creates the
//! shared header + frame-ready event + ring of keyed-mutex textures, and the driver only **OPENS** them.
//! The driver writes its actual render-adapter LUID + a status code back into the host-created header (our
//! only driver-visibility channel: UMDF hides OutputDebugString in ETW and the token can't write files),
//! then copies each acquired swap-chain surface into the next ring slot and signals the host.
//!
//! Host counterpart: `crates/punktfunk-host/src/capture/idd_push.rs`. The shared `SharedHeader` layout,
//! the [`FrameToken`] packing, the `Global\` object-name scheme, the `MAGIC`/`RING_LEN` and the
//! `DRV_STATUS_*` codes are NOT hand-duplicated here: both sides `use pf_vdisplay_proto::frame::*`, which
//! OWNS the contract (with `const` size asserts so any drift is a compile error).
//!
//! Ported from the proven oracle (`packaging/windows/vdisplay-driver/pf-vdisplay/src/frame_transport.rs`).
//! Differences from the oracle:
//! * the layout/consts/names/token come from `pf_vdisplay_proto::frame` instead of being re-declared;
//! * `dbglog!` replaces `log::info!`;
//! * the optional fixed-name `Global\pfvd-dbg` `DebugBlock` bring-up channel is SKIPPED (not on the data
//!   path). FOLLOW-UP: if the host bring-up diagnostics are needed again, port the oracle's `DebugBlock`
//!   here too (it is owned by `idd_push.rs`, not the proto).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use pf_vdisplay_proto::frame::{
    DRV_STATUS_NO_DEVICE1, DRV_STATUS_OPENED, DRV_STATUS_TEX_FAIL, FrameToken, MAGIC, RING_LEN,
    SharedHeader, event_name, header_name, texture_name,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;
use windows::Win32::System::Memory::{
    FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW,
    UnmapViewOfFile,
};
use windows::Win32::System::Threading::{OpenEventW, SYNCHRONIZATION_ACCESS_RIGHTS, SetEvent};
use windows::core::{HSTRING, Interface};

/// `DXGI_SHARED_RESOURCE_READ | _WRITE` — passed to `OpenSharedResourceByName` (matches the host's
/// `CreateSharedHandle` access). Kept local: it is a `OpenSharedResourceByName` arg, not part of the
/// proto contract. (Same value the host uses in `idd_push.rs`.)
const DXGI_SHARED_RESOURCE_RW: u32 = 0x8000_0000 | 0x1;
/// SYNCHRONIZE | EVENT_MODIFY_STATE — the driver does not wait on the event, only SIGNALS it.
const EVENT_ACCESS: u32 = 0x0010_0000 | 0x0002;
/// `WAIT_TIMEOUT` as an HRESULT — `AcquireSync` returns this when the slot is held by the consumer.
const WAIT_TIMEOUT_HRESULT: i32 = 0x0000_0102;

struct Slot {
    tex: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
}

/// Publishes acquired swap-chain surfaces into the HOST-created ring. Owned by the swap-chain processor
/// thread; attached lazily once the host has created the shared objects.
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
    /// detects that so `run_core` re-attaches to the new-format textures instead of dropping every frame.
    generation: u32,
}

// SAFETY: created and used only on the swap-chain processor thread.
unsafe impl Send for FramePublisher {}

impl FramePublisher {
    /// Try ONCE to attach to the host-created shared objects. Returns `Err` cheaply if the host hasn't
    /// created/published them yet — the drain loop retries periodically, so a non-IDD-push session just
    /// keeps draining with no stall. All early-return paths clean up the handles/mapping they opened
    /// explicitly (raw-handle style, no RAII — matches the rest of this driver).
    pub fn try_open(
        target_id: u32,
        render_luid_low: u32,
        render_luid_high: i32,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> windows::core::Result<Self> {
        // 1. Open the host-created header (RW). Err if the host hasn't created it yet.
        // SAFETY: a plain Win32 call; the name HSTRING is valid for the call (`?` returns on failure).
        let map = unsafe {
            OpenFileMappingW(
                FILE_MAP_ALL_ACCESS.0,
                false,
                &HSTRING::from(header_name(target_id)),
            )?
        };
        // SAFETY: `map` is the just-opened file mapping; mapping size_of::<SharedHeader>() bytes of it
        // (the host created the mapping at >= that size). The null `view.Value` is checked below.
        let view = unsafe {
            MapViewOfFile(
                map,
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                core::mem::size_of::<SharedHeader>(),
            )
        };
        if view.Value.is_null() {
            // SAFETY: `map` is the just-opened mapping handle, closed once here on the error path.
            unsafe {
                let _ = CloseHandle(map);
            }
            return Err(windows::core::Error::from_win32());
        }
        let header = view.Value.cast::<SharedHeader>();

        // 2. Report our render adapter to the host immediately (lets it detect a mismatch).
        // SAFETY: `header` points to the mapped, non-null host header (>= size_of::<SharedHeader>() bytes);
        // these scalar writes are within it. The host opened the section with a permissive SDDL for us.
        unsafe {
            (*header).driver_render_luid_low = render_luid_low;
            (*header).driver_render_luid_high = render_luid_high;
        }

        // 3. The host sets magic==MAGIC only once the ring textures exist. Not ready → retry later.
        // SAFETY: `header` is the mapped host header; `magic` lives within it and is read atomically
        // (Acquire) to pair with the host's Release store once the ring textures are published.
        let magic = unsafe {
            (*(core::ptr::addr_of!((*header).magic) as *const AtomicU32)).load(Ordering::Acquire)
        };
        if magic != MAGIC {
            // SAFETY: `header`/`map` are the live mapped view + handle; unmapped + closed once on this path.
            unsafe {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: header.cast(),
                });
                let _ = CloseHandle(map);
            }
            return Err(windows::core::Error::from_win32());
        }
        // SAFETY: `header` is the mapped host header; these scalar fields live within it.
        let (generation, ring_len) =
            unsafe { ((*header).generation, (*header).ring_len.min(RING_LEN)) };

        // 4. Open the event (SYNCHRONIZE | EVENT_MODIFY_STATE so we can SetEvent).
        // SAFETY: a plain Win32 call; the name HSTRING is valid for the call.
        let event = match unsafe {
            OpenEventW(
                SYNCHRONIZATION_ACCESS_RIGHTS(EVENT_ACCESS),
                false,
                &HSTRING::from(event_name(target_id)),
            )
        } {
            Ok(e) => e,
            Err(e) => {
                // SAFETY: `header`/`map` are the live mapped view + handle; unmapped + closed once here.
                unsafe {
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: header.cast(),
                    });
                    let _ = CloseHandle(map);
                }
                return Err(e);
            }
        };

        // 5. Open device1 + the ring textures the host created (same render adapter required).
        let device1: ID3D11Device1 = match device.cast() {
            Ok(d) => d,
            Err(e) => {
                // SAFETY: `header` is the mapped host header (status write within it); `event`/`map` are the
                // live handles, all released once on this error path.
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
        for k in 0..ring_len {
            let name = HSTRING::from(texture_name(target_id, generation, k));
            // SAFETY: `device1` is a live ID3D11Device1; the name HSTRING is valid for the call.
            let opened: windows::core::Result<ID3D11Texture2D> =
                unsafe { device1.OpenSharedResourceByName(&name, DXGI_SHARED_RESOURCE_RW) };
            match opened {
                Ok(tex) => match tex.cast::<IDXGIKeyedMutex>() {
                    Ok(mutex) => slots.push(Slot { tex, mutex }),
                    Err(e) => {
                        // SAFETY: `header` is the mapped host header (status writes within it); `event`/`map`
                        // are the live handles, all released once on this error path.
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
                },
                Err(e) => {
                    // Most likely a render-adapter mismatch (the host made the textures on a different
                    // GPU than the swap-chain renders on). Tell the host so it can report it.
                    // SAFETY: `header` is the mapped host header (status writes within it); `event`/`map`
                    // are the live handles, all released once on this error path.
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
        }

        // SAFETY: `header` is the mapped host header; the status field lives within it.
        unsafe {
            (*header).driver_status = DRV_STATUS_OPENED;
        }
        dbglog!(
            "[pf-vd] frame-push(driver): attached to host ring gen {generation} ({ring_len} slots)"
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
            generation,
        })
    }

    #[inline]
    fn latest_cell(&self) -> &AtomicU64 {
        // SAFETY: `self.header` stays mapped for the publisher's lifetime (unmapped only in Drop); the
        // `latest` field lives within it and is naturally aligned, so this AtomicU64 reference is valid.
        unsafe { &*(core::ptr::addr_of!((*self.header).latest) as *const AtomicU64) }
    }

    /// True once the host has recreated the ring (bumped the header generation) — e.g. the display's HDR
    /// mode flipped, so the ring format changed (FP16 ⇄ BGRA) and the texture names now carry a new
    /// generation. `run_core` drops the publisher on this so it re-attaches to the new ring.
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
        if desc.Format.0 as u32 != self.ring_format {
            return;
        }
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
                    // SAFETY: `self.event` is the live host-created frame-ready event we opened with
                    // EVENT_MODIFY_STATE; signalling it wakes the host consumer.
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
        // handles.
        self.slots.clear();
        // SAFETY: drop runs once; `self.header` (if non-null) is the live mapped view and `self.event`/
        // `self.map` are the live handles this publisher opened — each unmapped/closed exactly once here.
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
