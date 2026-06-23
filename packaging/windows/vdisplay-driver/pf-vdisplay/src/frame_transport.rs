//! P2 direct frame push — DRIVER side. The restricted WUDFHost token canNOT create named kernel
//! objects (proven on the RTX box: it can't even write a world-writable file), so — exactly like the
//! gamepad UMDF drivers (`crates/punktfunk-host/src/inject/dualsense_windows.rs`: *"the host creates
//! the section, privileged, with a permissive SDDL so the WUDFHost can open it; the driver maps it"*)
//! — the **host** creates the shared header + frame-ready event + ring of keyed-mutex textures, and
//! the driver only **OPENS** them. The driver writes its actual render-adapter LUID + a status code
//! back into the host-created header (our only driver-visibility channel: UMDF hides OutputDebugString
//! in ETW and the token can't write files), then copies each acquired swap-chain surface into the next
//! ring slot and signals the host.
//!
//! Host counterpart: `crates/punktfunk-host/src/capture/idd_push.rs` — [`SharedHeader`], [`MAGIC`],
//! [`RING_LEN`], the driver-status codes and the `Global\` object-name scheme are DUPLICATED
//! byte-identically there.

use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use log::info;
use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Threading::{OpenEventW, SetEvent, SYNCHRONIZATION_ACCESS_RIGHTS};

// --- kept byte-identical with the host (idd_push.rs) ---
pub const MAGIC: u32 = 0x4456_4650;
/// Kept for parity with the host's duplicated protocol header (the host writes it).
#[allow(dead_code)]
pub const VERSION: u32 = 1;
/// Ring slots. 6 (was 3) gives ample headroom so this 0 ms-timeout publish always finds a free slot
/// while the host briefly holds one across the convert/copy into its output ring and the depth-2
/// pipelined encode runs. MUST equal the host's `RING_LEN` (idd_push.rs) — both are rebuilt together;
/// a mismatch corrupts the slot mapping.
pub const RING_LEN: u32 = 6;
const DXGI_SHARED_RESOURCE_RW: u32 = 0x8000_0000 | 0x1;
/// SYNCHRONIZE | EVENT_MODIFY_STATE — the driver waits on (no) and SIGNALS the event.
const EVENT_ACCESS: u32 = 0x0010_0000 | 0x0002;
const WAIT_TIMEOUT_HRESULT: i32 = 0x0000_0102;

/// `driver_status` values the driver writes into the host header (the host logs them on a timeout).
/// `NONE` is the host's initial value (kept for parity).
#[allow(dead_code)]
pub const DRV_STATUS_NONE: u32 = 0;
pub const DRV_STATUS_OPENED: u32 = 1;
pub const DRV_STATUS_TEX_FAIL: u32 = 2;
pub const DRV_STATUS_NO_DEVICE1: u32 = 3;

#[repr(C)]
pub struct SharedHeader {
    pub magic: u32,
    pub version: u32,
    pub generation: u32,
    pub ring_len: u32,
    pub width: u32,
    pub height: u32,
    pub dxgi_format: u32,
    pub _pad: u32,
    /// `(seq << 8) | slot` — DRIVER-written after each copy; host loads it `Acquire`.
    pub latest: u64,
    pub qpc_pts: u64,
    /// DRIVER-written: the adapter the swap-chain actually renders on (so the host can detect a
    /// mismatch with the textures it created and report it).
    pub driver_render_luid_low: u32,
    pub driver_render_luid_high: i32,
    /// DRIVER-written status (visibility channel).
    pub driver_status: u32,
    pub driver_status_detail: u32,
}

pub fn hdr_name(target_id: u32) -> String {
    format!("Global\\pfvd-hdr-{target_id}")
}
pub fn evt_name(target_id: u32) -> String {
    format!("Global\\pfvd-evt-{target_id}")
}
pub fn tex_name(target_id: u32, generation: u32, slot: u32) -> String {
    format!("Global\\pfvd-tex-{target_id}-{generation}-{slot}")
}
// --------------------------------------------------------

// ===== Bring-up debug channel (fixed-name, host-created) =====
// UMDF hides the driver's OutputDebugString (ETW) and the restricted token can't write files, so this
// fixed-name `Global\pfvd-dbg` block — created by the host with the permissive SDDL — is how the driver
// reports what it's doing, INDEPENDENT of the per-target header (which is the thing under test). The
// host reads + logs these counters. Duplicated in `idd_push.rs`.
#[repr(C)]
pub struct DebugBlock {
    pub magic: u32,
    /// ++ each `run_core` entry — proves the swap-chain processor runs at all.
    pub run_core_entries: u32,
    /// The `target_id` the driver resolved for naming (mismatch vs the host = the bug).
    pub resolved_target_id: u32,
    /// ++ each header-open attempt.
    pub header_open_attempts: u32,
    /// Last header-open error (win32/HRESULT).
    pub last_open_error: u32,
    /// 1 once the driver opened the per-target header.
    pub header_opened: u32,
    pub render_luid_low: u32,
    pub render_luid_high: i32,
    /// ++ each acquired swap-chain frame — proves frames flow (or the display is idle).
    pub frames_acquired: u32,
    pub _pad: u32,
}

static DBG_PTR: AtomicPtr<DebugBlock> = AtomicPtr::new(std::ptr::null_mut());

/// Map the host-created debug block on first use (fixed name). Returns null until the host creates it.
fn dbg_block() -> *mut DebugBlock {
    let p = DBG_PTR.load(Ordering::Acquire);
    if !p.is_null() {
        return p;
    }
    let Ok(map) = (unsafe {
        OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, &HSTRING::from("Global\\pfvd-dbg"))
    }) else {
        return std::ptr::null_mut();
    };
    let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, std::mem::size_of::<DebugBlock>()) };
    if view.Value.is_null() {
        unsafe {
            let _ = CloseHandle(map);
        }
        return std::ptr::null_mut();
    }
    let np = view.Value.cast::<DebugBlock>();
    match DBG_PTR.compare_exchange(std::ptr::null_mut(), np, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => np, // we win; intentionally leak the handle (diagnostic, process-lifetime)
        Err(existing) => {
            unsafe {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(map);
            }
            existing
        }
    }
}

pub fn dbg_run_core_entry() {
    let p = dbg_block();
    if !p.is_null() {
        unsafe {
            (*(std::ptr::addr_of_mut!((*p).run_core_entries) as *const AtomicU32))
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn dbg_frame() {
    let p = dbg_block();
    if !p.is_null() {
        unsafe {
            (*(std::ptr::addr_of_mut!((*p).frames_acquired) as *const AtomicU32))
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Record the target id + render LUID the driver will use to name the shared objects.
pub fn dbg_set_target(target_id: u32, render_luid_low: u32, render_luid_high: i32) {
    let p = dbg_block();
    if !p.is_null() {
        unsafe {
            (*p).resolved_target_id = target_id;
            (*p).render_luid_low = render_luid_low;
            (*p).render_luid_high = render_luid_high;
        }
    }
}

/// Record a header-open attempt + its error (0 = success).
pub fn dbg_header_attempt(error: u32, opened: bool) {
    let p = dbg_block();
    if !p.is_null() {
        unsafe {
            (*(std::ptr::addr_of_mut!((*p).header_open_attempts) as *const AtomicU32))
                .fetch_add(1, Ordering::Relaxed);
            (*p).last_open_error = error;
            if opened {
                (*p).header_opened = 1;
            }
        }
    }
}

struct Slot {
    tex: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
}

/// Publishes acquired swap-chain surfaces into the HOST-created ring. Owned by the swap-chain
/// processor thread; attached lazily once the host has created the shared objects.
pub struct FramePublisher {
    context: ID3D11DeviceContext,
    map: HANDLE,
    header: *mut SharedHeader,
    event: HANDLE,
    slots: Vec<Slot>,
    next: u32,
    seq: u64,
    /// The host-created ring textures' DXGI format (from the shared header). A swap-chain surface whose
    /// format differs (e.g. an FP16 HDR frame vs a BGRA ring) is dropped in `publish` — CopyResource
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
    /// created/published them yet — the drain loop retries periodically, so a non-IDD-push session
    /// just keeps draining with no stall.
    pub fn try_open(
        target_id: u32,
        render_luid_low: u32,
        render_luid_high: i32,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> windows::core::Result<Self> {
        // 1. Open the host-created header (RW). Err if the host hasn't created it yet.
        let map = unsafe {
            OpenFileMappingW(
                FILE_MAP_ALL_ACCESS.0,
                false,
                &HSTRING::from(hdr_name(target_id)),
            )?
        };
        let view =
            unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, std::mem::size_of::<SharedHeader>()) };
        if view.Value.is_null() {
            unsafe {
                let _ = CloseHandle(map);
            }
            return Err(windows::core::Error::from_win32());
        }
        let header = view.Value.cast::<SharedHeader>();

        // 2. Report our render adapter to the host immediately (lets it detect a mismatch).
        unsafe {
            (*header).driver_render_luid_low = render_luid_low;
            (*header).driver_render_luid_high = render_luid_high;
        }

        // 3. The host sets magic==MAGIC only once the ring textures exist. Not ready → retry later.
        let magic =
            unsafe { (*(std::ptr::addr_of!((*header).magic) as *const AtomicU32)).load(Ordering::Acquire) };
        if magic != MAGIC {
            unsafe {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: header.cast() });
                let _ = CloseHandle(map);
            }
            return Err(windows::core::Error::from_win32());
        }
        let (generation, ring_len) =
            unsafe { ((*header).generation, (*header).ring_len.min(RING_LEN)) };

        // 4. Open the event (SYNCHRONIZE | EVENT_MODIFY_STATE so we can SetEvent).
        let event = match unsafe {
            OpenEventW(
                SYNCHRONIZATION_ACCESS_RIGHTS(EVENT_ACCESS),
                false,
                &HSTRING::from(evt_name(target_id)),
            )
        } {
            Ok(e) => e,
            Err(e) => {
                unsafe {
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: header.cast() });
                    let _ = CloseHandle(map);
                }
                return Err(e);
            }
        };

        // 5. Open device1 + the ring textures the host created (same render adapter required).
        let device1: ID3D11Device1 = match device.cast() {
            Ok(d) => d,
            Err(e) => {
                unsafe {
                    (*header).driver_status = DRV_STATUS_NO_DEVICE1;
                    let _ = CloseHandle(event);
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: header.cast() });
                    let _ = CloseHandle(map);
                }
                return Err(e);
            }
        };
        let mut slots = Vec::new();
        for k in 0..ring_len {
            let name = HSTRING::from(tex_name(target_id, generation, k));
            let opened: windows::core::Result<ID3D11Texture2D> =
                unsafe { device1.OpenSharedResourceByName(&name, DXGI_SHARED_RESOURCE_RW) };
            match opened {
                Ok(tex) => match tex.cast::<IDXGIKeyedMutex>() {
                    Ok(mutex) => slots.push(Slot { tex, mutex }),
                    Err(e) => {
                        unsafe {
                            (*header).driver_status = DRV_STATUS_TEX_FAIL;
                            (*header).driver_status_detail = e.code().0 as u32;
                            let _ = CloseHandle(event);
                            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: header.cast() });
                            let _ = CloseHandle(map);
                        }
                        return Err(e);
                    }
                },
                Err(e) => {
                    // Most likely a render-adapter mismatch (the host made the textures on a different
                    // GPU than the swap-chain renders on). Tell the host so it can report it.
                    unsafe {
                        (*header).driver_status = DRV_STATUS_TEX_FAIL;
                        (*header).driver_status_detail = e.code().0 as u32;
                        let _ = CloseHandle(event);
                        let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: header.cast() });
                        let _ = CloseHandle(map);
                    }
                    return Err(e);
                }
            }
        }

        unsafe {
            (*header).driver_status = DRV_STATUS_OPENED;
        }
        info!("frame-push(driver): attached to host ring gen {generation} ({ring_len} slots)");
        Ok(Self {
            context: context.clone(),
            map,
            header,
            event,
            slots,
            next: 0,
            seq: 0,
            ring_format: unsafe { (*header).dxgi_format },
            generation,
        })
    }

    #[inline]
    fn latest_cell(&self) -> &AtomicU64 {
        unsafe { &*(std::ptr::addr_of!((*self.header).latest) as *const AtomicU64) }
    }

    /// True once the host has recreated the ring (bumped the header generation) — e.g. the display's
    /// HDR mode flipped, so the ring format changed (FP16 ⇄ BGRA) and the texture names now carry a new
    /// generation. `run_core` drops the publisher on this so it re-attaches to the new ring.
    pub fn is_stale(&self) -> bool {
        let cur = unsafe {
            (*(std::ptr::addr_of!((*self.header).generation) as *const AtomicU32))
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
        // B2 format guard: CopyResource needs the surface + ring textures to share a DXGI format. Drop
        // a frame that doesn't match (e.g. an FP16 HDR surface arriving while the ring is still BGRA,
        // before B3 makes the ring FP16) instead of corrupting / failing the copy.
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { surface.GetDesc(&mut desc) };
        if desc.Format.0 as u32 != self.ring_format {
            return;
        }
        let start = self.next;
        for attempt in 0..ring_len {
            let slot = (start + attempt) % ring_len;
            let s = &self.slots[slot as usize];
            match unsafe { s.mutex.AcquireSync(0, 0) } {
                Ok(()) => {
                    unsafe {
                        self.context.CopyResource(&s.tex, surface);
                        let _ = s.mutex.ReleaseSync(0);
                    }
                    self.seq = self.seq.wrapping_add(1);
                    // `latest` = (generation << 40) | (seq << 8) | slot. Stamping the generation lets the
                    // host REJECT a publish from a stale ring (an old-generation publisher racing the
                    // host's mid-session ring recreate) so it never consumes an unwritten new-ring slot.
                    let latest = (u64::from(self.generation) << 40)
                        | ((self.seq & 0xFFFF_FFFF) << 8)
                        | u64::from(slot & 0xff);
                    self.latest_cell().store(latest, Ordering::Release);
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
        self.slots.clear();
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
