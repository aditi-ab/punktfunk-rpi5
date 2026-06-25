//! The swap-chain processor (STEP 5): a worker thread that DRAINS the IddCx swap-chain so the virtual
//! monitor stays a usable display.
//!
//! The OS presents the composited desktop to the driver through a swap-chain; the driver MUST consume
//! it (acquire → finished-processing) or the monitor stalls. STEP 5 binds our render device to the
//! swap-chain (`IddCxSwapChainSetDevice`) and loops acquire/finish, discarding each frame. It does NOT
//! publish frames to the host — that is STEP 6 (the `CopyResource` of `out.MetaData.pSurface` into a
//! shared ring), deliberately omitted here.
//!
//! Ported from the proven oracle (`packaging/windows/vdisplay-driver/pf-vdisplay/src/
//! swap_chain_processor.rs`) onto wdk-sys + wdk-iddcx. The oracle's `wdf_umdf`/`wdf_umdf_sys` are
//! replaced by `wdk_sys::iddcx::*` + the `wdk_iddcx` DDI wrappers. Those wrappers return a RAW
//! `NTSTATUS` (`i32`) that is HRESULT-shaped for the swap-chain DDIs, so we classify it by hand
//! (`hr >= 0` = success; `0x8000_000A` = E_PENDING; `hr < 0 && != E_PENDING` = error) rather than with
//! `nt_success`. The publisher + `render_luid_low/high` params are dropped (STEP 6).

use std::{
    mem::size_of,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use wdk_sys::iddcx::{
    IDARG_IN_RELEASEANDACQUIREBUFFER2, IDARG_IN_SWAPCHAINSETDEVICE,
    IDARG_OUT_RELEASEANDACQUIREBUFFER2, IDDCX_SWAPCHAIN,
};
// `HANDLE` is the shared wdk-sys typedef (`crate::types`) re-used by the iddcx bindings — take it from
// the crate root, which is guaranteed to export it (the iddcx module only re-exports it if bindgen
// re-declared it there). It is the same type as `IDARG_IN_SETSWAPCHAIN.hNextSurfaceAvailable`.
use wdk_sys::{HANDLE, NTSTATUS, WDFOBJECT, call_unsafe_wdf_function_binding};
use windows::{
    Win32::{
        Foundation::HANDLE as WHANDLE,
        Graphics::Dxgi::IDXGIDevice,
        System::Threading::{
            AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, WaitForSingleObject,
        },
    },
    core::{Interface, w},
};

use crate::direct_3d_device::Direct3DDevice;

/// E_PENDING — `ReleaseAndAcquireBuffer2` returns this (HRESULT-shaped) when the swap-chain is valid but
/// DWM has composed no new frame yet; wait on the surface-available event and retry.
const E_PENDING: u32 = 0x8000_000A;
/// `WAIT_TIMEOUT` from `WaitForSingleObject` (defined locally to avoid pulling a windows-crate constant
/// type into the comparison — the raw `WAIT_EVENT.0` is just a `u32`).
const WAIT_TIMEOUT_U32: u32 = 0x0000_0102;

/// HRESULT-shaped success test for the swap-chain DDIs (raw `NTSTATUS`/HRESULT: success iff non-negative).
#[inline]
fn hr_success(hr: NTSTATUS) -> bool {
    hr >= 0
}

/// A minimal newtype to move a raw pointer / handle across the thread boundary. The wrapped value is a
/// raw IddCx swap-chain handle or an event HANDLE (both raw pointers, framework-managed) — sending them
/// to the worker is sound because only this thread touches them and the framework synchronises lifetime.
struct Sendable<T>(T);
// SAFETY: see the type doc — the wrapped raw handle is owned by the worker for its lifetime.
unsafe impl<T> Send for Sendable<T> {}

pub struct SwapChainProcessor {
    terminate: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

// SAFETY: Raw ptr is managed by external library; access is serialised by the worker thread + the
// terminate flag.
unsafe impl Send for SwapChainProcessor {}
unsafe impl Sync for SwapChainProcessor {}

impl SwapChainProcessor {
    pub fn new() -> Self {
        Self {
            terminate: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    pub fn run(
        &mut self,
        swap_chain: IDDCX_SWAPCHAIN,
        device: Arc<Direct3DDevice>,
        available_buffer_event: HANDLE,
        target_id: u32,
    ) {
        let available_buffer_event = Sendable(available_buffer_event);
        let swap_chain = Sendable(swap_chain);
        let terminate = self.terminate.clone();

        let join_handle = thread::spawn(move || {
            // Rust 2021 disjoint closure captures would otherwise grab the raw `swap_chain.0` /
            // `available_buffer_event.0` FIELDS directly (defeating the `Sendable` Send wrapper, since the
            // inner `*mut IDDCX_SWAPCHAIN__` / `HANDLE` are `!Send`). Rebind the WHOLE wrappers here so the
            // closure captures them as `Sendable<_>` (which IS `Send`), then unwrap from the locals.
            let swap_chain = swap_chain;
            let available_buffer_event = available_buffer_event;
            // It is very important to prioritize this thread by making use of the Multimedia Scheduler
            // Service. It will intelligently prioritize the thread for improved throughput in high
            // CPU-load scenarios.
            let mut av_task = 0u32;
            let res = unsafe { AvSetMmThreadCharacteristicsW(w!("Distribution"), &mut av_task) };
            let Ok(av_handle) = res else {
                dbglog!("[pf-vd] swap-chain: failed to prioritize thread: {res:?}");
                return;
            };

            Self::run_core(
                swap_chain.0,
                &device,
                available_buffer_event.0,
                &terminate,
                target_id,
            );

            dbglog!(
                "[pf-vd] swap-chain run_core RETURNED (target={target_id}) — deleting swap-chain, device drops next"
            );

            // Delete the swap-chain WDF object BEFORE the `Arc<Direct3DDevice>` drops (the swap-chain
            // referenced our device). `WdfObjectDelete` takes a WDFOBJECT.
            // SAFETY: `swap_chain` is a live IddCx swap-chain handle; we own the sole reference here and
            // the drain loop has exited.
            unsafe {
                call_unsafe_wdf_function_binding!(WdfObjectDelete, swap_chain.0 as WDFOBJECT);
            }

            // Revert the thread to normal once it's done.
            let res = unsafe { AvRevertMmThreadCharacteristics(av_handle) };
            if let Err(e) = res {
                dbglog!("[pf-vd] swap-chain: failed to revert prioritized thread: {e:?}");
            }
        });

        self.thread = Some(join_handle);
    }

    fn run_core(
        swap_chain: IDDCX_SWAPCHAIN,
        device: &Direct3DDevice,
        available_buffer_event: HANDLE,
        terminate: &AtomicBool,
        target_id: u32,
    ) {
        // SetDevice fails (0x887A0026, FACILITY_DXGI) when the monitor briefly flaps INACTIVE during
        // topology activation — the OS unassigns + re-assigns the swap-chain, and a fresh run_core thread
        // can lose the race to the unassign. Retry briefly so a stable re-assign binds the device instead
        // of giving up on the first transient failure. `terminate` (set when the OS unassigns + drops the
        // processor) breaks us out promptly.
        //
        // Cast to IDXGIDevice ONCE and BORROW it to the swap-chain across all retries. Re-casting +
        // `into_raw()`'ing on EVERY attempt — and a flapping monitor fails several attempts per session —
        // orphans an IDXGIDevice reference per failure, pinning the D3D device (and its ~dozen worker
        // threads + tens of MB of VRAM) so it is NEVER freed when the processor drops. `as_raw()` keeps
        // our single reference (released right after the loop); IddCx AddRefs its own on success, and
        // `device` keeps the object alive for the drain loop regardless.
        let dxgi_device = match device.device.cast::<IDXGIDevice>() {
            Ok(d) => d,
            Err(e) => {
                dbglog!("[pf-vd] swap-chain: failed to cast ID3D11Device to IDXGIDevice: {e:?}");
                return;
            }
        };
        // Built zeroed + field-assigned (driver style) — robust against a bindgen field-set difference.
        let mut set_device: IDARG_IN_SWAPCHAINSETDEVICE = unsafe { core::mem::zeroed() };
        set_device.pDevice = dxgi_device.as_raw().cast();
        let mut set_ok = false;
        let mut terminated = false;
        for attempt in 0..60u32 {
            if terminate.load(Ordering::Relaxed) {
                dbglog!(
                    "[pf-vd] swap-chain run_core: terminated during SetDevice (attempt {attempt}, target={target_id})"
                );
                terminated = true;
                break;
            }
            // SAFETY: driver is loaded; `swap_chain` is valid; `set_device` points to valid local storage.
            let hr = unsafe { wdk_iddcx::IddCxSwapChainSetDevice(swap_chain, &set_device) };
            if hr_success(hr) {
                set_ok = true;
                dbglog!(
                    "[pf-vd] swap-chain run_core: SetDevice OK (target={target_id}, attempt={attempt}) — entering drain loop"
                );
                break;
            }
            if attempt == 0 {
                dbglog!(
                    "[pf-vd] swap-chain run_core: SetDevice attempt 0 failed ({hr:#x}) — retrying up to 60x@50ms (monitor may be flapping)"
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
        // Release our borrowed device reference — IddCx holds its own now, or we gave up. (Explicit drop
        // so NLL can't release it mid-loop while the swap-chain still references the raw ptr.)
        drop(dxgi_device);
        if !set_ok {
            if !terminated {
                dbglog!(
                    "[pf-vd] swap-chain run_core: SetDevice never succeeded after retries (target={target_id}) — giving up"
                );
            }
            return;
        }

        let mut logged_pending = false;
        let mut logged_frame = false;
        loop {
            // Check terminate at the TOP, every iteration. The success branch below does NOT re-check it,
            // so during a CONTINUOUS frame burst (DWM rendering the freshly-activated desktop) a thread the
            // OS unassigns — or that the processor is dropping — never sees the flag and loops on, pinning
            // its D3D device (and ~36 NVIDIA worker threads). That is THE reconnect leak; it only
            // reproduced at full speed (E_PENDING gaps DO check terminate and masked it under a debugger).
            // Without this, `SwapChainProcessor::drop`'s join can also block until the burst ends.
            if terminate.load(Ordering::Relaxed) {
                break;
            }

            // ...Buffer2 is required once CAN_PROCESS_FP16 is set. AcquireSystemMemoryBuffer=FALSE keeps
            // the GPU surface (out.MetaData.pSurface). STEP 5 only drains — it does NOT publish the
            // surface (STEP 6 will). Built zeroed + field-assigned (driver style) so a bindgen field-set
            // difference can't break a positional struct literal.
            let mut in_args: IDARG_IN_RELEASEANDACQUIREBUFFER2 = unsafe { core::mem::zeroed() };
            #[allow(clippy::cast_possible_truncation)]
            {
                in_args.Size = size_of::<IDARG_IN_RELEASEANDACQUIREBUFFER2>() as u32;
            }
            in_args.AcquireSystemMemoryBuffer = 0;
            // `core::mem::zeroed()` (not `::default()`) — consistent with every other IddCx out-struct
            // in this driver, and robust whether or not bindgen derives `Default` for this type (its
            // `MetaData` field carries a raw `pSurface` pointer + union which can suppress the derive).
            let mut buffer: IDARG_OUT_RELEASEANDACQUIREBUFFER2 = unsafe { core::mem::zeroed() };
            // SAFETY: driver is loaded; `swap_chain` is valid; in/out point to valid local storage.
            let hr: NTSTATUS = unsafe {
                wdk_iddcx::IddCxSwapChainReleaseAndAcquireBuffer2(
                    swap_chain,
                    &mut in_args,
                    &mut buffer,
                )
            };

            if (hr as u32) == E_PENDING {
                if !logged_pending {
                    dbglog!(
                        "[pf-vd] swap-chain run_core: E_PENDING (target={target_id}) — swap-chain valid but DWM has composed NO frame yet"
                    );
                    logged_pending = true;
                }
                // SAFETY: `available_buffer_event` is the framework-provided surface-available event.
                let wait_result =
                    unsafe { WaitForSingleObject(WHANDLE(available_buffer_event.cast()), 16).0 };

                // thread requested an end
                if terminate.load(Ordering::Relaxed) {
                    break;
                }

                // WAIT_OBJECT_0 | WAIT_TIMEOUT
                if matches!(wait_result, 0 | WAIT_TIMEOUT_U32) {
                    // We have a new buffer (or timed out), so try the AcquireBuffer again.
                    continue;
                }

                // The wait was cancelled or something unexpected happened.
                break;
            } else if hr_success(hr) {
                if !logged_frame {
                    dbglog!(
                        "[pf-vd] swap-chain run_core: FIRST FRAME acquired (target={target_id}) — DWM IS compositing the virtual display!"
                    );
                    logged_frame = true;
                }
                // STEP 6 publishes `buffer.MetaData.pSurface` into the shared ring HERE (the surface is
                // valid until the next ReleaseAndAcquire). STEP 5 only drains, so we immediately finish
                // the frame.
                // SAFETY: driver is loaded; `swap_chain` is valid.
                let hr = unsafe { wdk_iddcx::IddCxSwapChainFinishedProcessingFrame(swap_chain) };
                if !hr_success(hr) {
                    break;
                }
            } else {
                // The swap-chain was likely abandoned (e.g. DXGI_ERROR_ACCESS_LOST) — exit the loop.
                break;
            }
        }
    }
}

impl Drop for SwapChainProcessor {
    fn drop(&mut self) {
        if let Some(handle) = self.thread.take() {
            // signal the worker to end
            self.terminate.store(true, Ordering::Relaxed);
            // wait until the worker is finished (it deletes the swap-chain object before returning)
            let _ = handle.join();
        }
    }
}
