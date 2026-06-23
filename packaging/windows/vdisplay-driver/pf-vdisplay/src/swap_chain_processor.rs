use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use log::{debug, error};
use wdf_umdf::{
    IddCxSwapChainFinishedProcessingFrame, IddCxSwapChainReleaseAndAcquireBuffer2,
    IddCxSwapChainSetDevice, WdfObjectDelete,
};
use wdf_umdf_sys::{
    HANDLE, IDARG_IN_RELEASEANDACQUIREBUFFER2, IDARG_IN_SWAPCHAINSETDEVICE,
    IDARG_OUT_RELEASEANDACQUIREBUFFER2, IDDCX_SWAPCHAIN, NTSTATUS, WAIT_TIMEOUT, WDFOBJECT,
};
use windows::{
    core::{w, Interface},
    Win32::{
        Foundation::HANDLE as WHANDLE,
        Graphics::{
            Direct3D11::ID3D11Texture2D,
            Dxgi::{IDXGIDevice, IDXGIResource},
        },
        System::Threading::{
            AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, WaitForSingleObject,
        },
    },
};

use crate::{
    direct_3d_device::Direct3DDevice,
    frame_transport::{
        dbg_frame, dbg_header_attempt, dbg_run_core_entry, dbg_set_target, FramePublisher,
    },
    helpers::Sendable,
};

pub struct SwapChainProcessor {
    terminate: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

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
        render_luid_low: u32,
        render_luid_high: i32,
    ) {
        let available_buffer_event = unsafe { Sendable::new(available_buffer_event) };
        let swap_chain = unsafe { Sendable::new(swap_chain) };
        let terminate = self.terminate.clone();

        let join_handle = thread::spawn(move || {
            // It is very important to prioritize this thread by making use of the Multimedia Scheduler Service.
            // It will intelligently prioritize the thread for improved throughput in high CPU-load scenarios.
            let mut av_task = 0u32;
            let res = unsafe { AvSetMmThreadCharacteristicsW(w!("Distribution"), &mut av_task) };
            let Ok(av_handle) = res else {
                error!("Failed to prioritize thread: {res:?}");
                return;
            };

            Self::run_core(
                *swap_chain,
                &device,
                *available_buffer_event,
                &terminate,
                target_id,
                render_luid_low,
                render_luid_high,
            );

            error!("run_core RETURNED (target={target_id}) — deleting swap-chain, device drops next");

            let res = unsafe { WdfObjectDelete(*swap_chain as WDFOBJECT) };
            if let Err(e) = res {
                error!("Failed to delete wdf object: {e:?}");
                return;
            }

            // Revert the thread to normal once it's done
            let res = unsafe { AvRevertMmThreadCharacteristics(av_handle) };
            if let Err(e) = res {
                error!("Failed to revert prioritize thread: {e:?}");
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
        render_luid_low: u32,
        render_luid_high: i32,
    ) {
        // P2 direct frame push: lazily ATTACH to the HOST-created shared ring. The restricted UMDF
        // token can't create named objects, so the host creates the header + event + textures and we
        // only OPEN them once they appear (`try_open`). Until then we just drain — exactly the P1
        // behaviour — so a non-IDD-push session never stalls. Retried every ~30 frames.
        let mut publisher: Option<FramePublisher> = None;
        let mut frames_since_try: u32 = u32::MAX; // attach attempt on the first acquired frame

        // Bring-up debug: prove run_core ran + record the target/render LUID we'll name objects with.
        dbg_run_core_entry();
        dbg_set_target(target_id, render_luid_low, render_luid_high);

        // SetDevice fails (0x887A0026, FACILITY_DXGI) when the monitor briefly flaps INACTIVE during
        // topology activation — the OS unassigns + re-assigns the swap-chain, and a fresh run_core thread
        // can lose the race to the unassign. Retry briefly so a stable re-assign binds the device instead
        // of giving up on the first transient failure. `terminate` (set when the OS unassigns + drops the
        // processor) breaks us out promptly.
        // Cast to IDXGIDevice ONCE and BORROW it to the swap-chain across all retries. The previous
        // code re-cast + `into_raw()`'d on EVERY attempt — and a flapping monitor fails several
        // attempts per session — so each failure orphaned one IDXGIDevice reference, pinning the D3D
        // device so it (and its ~dozen D3D worker threads + tens of MB of VRAM) was NEVER freed when
        // the processor dropped. That leaked ~71 threads / ~57 MB VRAM per reconnect until the driver
        // choked and sessions fell to 0 bytes. `as_raw()` keeps our single reference (released right
        // after the loop); IddCx AddRefs its own on success, and `device` keeps the object alive for
        // the drain loop regardless.
        let dxgi_device = match device.device.cast::<IDXGIDevice>() {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to cast ID3D11Device to IDXGIDevice: {e:?}");
                return;
            }
        };
        let set_device = IDARG_IN_SWAPCHAINSETDEVICE {
            pDevice: dxgi_device.as_raw().cast(),
        };
        let mut set_ok = false;
        let mut terminated = false;
        for attempt in 0..60u32 {
            if terminate.load(Ordering::Relaxed) {
                error!("run_core: terminated during SetDevice (attempt {attempt}, target={target_id})");
                terminated = true;
                break;
            }
            let res = unsafe { IddCxSwapChainSetDevice(swap_chain, &set_device) };
            if res.is_ok() {
                set_ok = true;
                error!("run_core: SetDevice OK (target={target_id}, attempt={attempt}) — entering drain loop");
                break;
            }
            if attempt == 0 {
                debug!("run_core: SetDevice attempt 0 failed ({res:?}) — retrying up to 60x@50ms (monitor may be flapping)");
            }
            thread::sleep(Duration::from_millis(50));
        }
        // Release our borrowed device reference — IddCx holds its own now, or we gave up. (Explicit
        // drop so NLL can't release it mid-loop while the swap-chain still references the raw ptr.)
        drop(dxgi_device);
        if !set_ok {
            if !terminated {
                error!("run_core: SetDevice never succeeded after retries (target={target_id}) — giving up");
            }
            return;
        }

        let mut logged_pending = false;
        let mut logged_frame = false;
        loop {
            // Check terminate at the TOP, every iteration. The success branch below does NOT re-check
            // it, so during a CONTINUOUS frame burst (DWM rendering the freshly-activated desktop) a
            // thread that the OS unassigns — or that `free_swap_chain_processor` is dropping — never
            // sees the flag and loops on, pinning its D3D device (and ~36 NVIDIA worker threads). That
            // is THE reconnect leak: it only reproduced at full speed, because cdb's pacing forced
            // E_PENDING gaps (which DO check terminate) and masked it. Without this, `SwapChainProcessor::drop`'s
            // join can also block until the burst ends.
            if terminate.load(Ordering::Relaxed) {
                break;
            }
            // The host recreates the shared ring (new format) mid-session when the display's HDR mode
            // flips — it bumps the header generation. Detect that and drop the publisher so we re-attach
            // to the new-format textures below; otherwise we'd keep CopyResource'ing into the stale ring,
            // whose format now mismatches the surface → the publish() format-guard drops every frame and
            // the stream freezes until the next swap-chain recreate.
            if publisher.as_ref().is_some_and(FramePublisher::is_stale) {
                publisher = None;
                frames_since_try = u32::MAX; // re-attach immediately
            }
            // Lazy-attach (rate-limited) at the loop TOP so we keep trying even while the display is
            // idle (E_PENDING / no frames presented yet), not only when a frame is acquired. `try_open`
            // is a cheap OpenFileMapping that fails fast until the host has created the ring.
            if publisher.is_none() {
                if frames_since_try >= 30 {
                    frames_since_try = 0;
                    match FramePublisher::try_open(
                        target_id,
                        render_luid_low,
                        render_luid_high,
                        &device.device,
                        &device.device_context,
                    ) {
                        Ok(p) => {
                            dbg_header_attempt(0, true);
                            publisher = Some(p);
                        }
                        Err(e) => dbg_header_attempt(e.code().0 as u32, false),
                    }
                } else {
                    frames_since_try += 1;
                }
            }

            // B2: ...Buffer2 is required once CAN_PROCESS_FP16 is set. AcquireSystemMemoryBuffer=FALSE
            // keeps the GPU surface (out.MetaData.pSurface). The surface format varies per-frame —
            // FP16 (R16G16B16A16_FLOAT) in HDR, BGRA in SDR — and the publisher's format guard handles
            // a frame that doesn't match the ring until B3 makes the ring FP16.
            let mut in_args = IDARG_IN_RELEASEANDACQUIREBUFFER2 {
                #[allow(clippy::cast_possible_truncation)]
                Size: std::mem::size_of::<IDARG_IN_RELEASEANDACQUIREBUFFER2>() as u32,
                AcquireSystemMemoryBuffer: 0,
            };
            let mut buffer = IDARG_OUT_RELEASEANDACQUIREBUFFER2::default();
            let hr: NTSTATUS = unsafe {
                IddCxSwapChainReleaseAndAcquireBuffer2(swap_chain, &mut in_args, &mut buffer).into()
            };

            #[allow(clippy::items_after_statements)]
            const E_PENDING: u32 = 0x8000_000A;
            if u32::from(hr) == E_PENDING {
                if !logged_pending {
                    error!("run_core: E_PENDING (target={target_id}) — swap-chain valid but DWM has composed NO frame yet");
                    logged_pending = true;
                }
                let wait_result =
                    unsafe { WaitForSingleObject(WHANDLE(available_buffer_event.cast()), 16).0 };

                // thread requested an end
                let should_terminate = terminate.load(Ordering::Relaxed);
                if should_terminate {
                    break;
                }

                // WAIT_OBJECT_0 | WAIT_TIMEOUT
                if matches!(wait_result, 0 | WAIT_TIMEOUT) {
                    // We have a new buffer, so try the AcquireBuffer again
                    continue;
                }

                // The wait was cancelled or something unexpected happened
                break;
            } else if hr.is_success() {
                if !logged_frame {
                    error!("run_core: FIRST FRAME acquired (target={target_id}) — DWM IS compositing the virtual display!");
                    logged_frame = true;
                }
                dbg_frame(); // bring-up: prove frames actually flow (vs an idle display)
                // This is the most performance-critical section of code in an IddCx driver. It's important that whatever
                // is done with the acquired surface be finished as quickly as possible.
                //
                // P2: copy the acquired surface into the shared ring BEFORE FinishedProcessingFrame
                // (the surface is valid until the next ReleaseAndAcquire). The pointer is BORROWED —
                // `from_raw_borrowed` does not take IddCx's refcount — and the GPU-side copy is ordered
                // before the consumer via the slot keyed mutex. (Attach happens at the loop top.)
                if let Some(pub_) = publisher.as_mut() {
                    let raw = buffer.MetaData.pSurface as *mut core::ffi::c_void;
                    if !raw.is_null() {
                        if let Some(res) = unsafe { IDXGIResource::from_raw_borrowed(&raw) } {
                            if let Ok(tex) = res.cast::<ID3D11Texture2D>() {
                                pub_.publish(&tex);
                            }
                        }
                    }
                }

                let hr = unsafe { IddCxSwapChainFinishedProcessingFrame(swap_chain) };

                if hr.is_err() {
                    break;
                }
            } else {
                // The swap-chain was likely abandoned (e.g. DXGI_ERROR_ACCESS_LOST), so exit the processing loop
                break;
            }
        }
    }
}

impl Drop for SwapChainProcessor {
    fn drop(&mut self) {
        if let Some(handle) = self.thread.take() {
            // send signal to end thread
            self.terminate.store(true, Ordering::Relaxed);

            // wait until thread is finished
            _ = handle.join();
        }
    }
}
