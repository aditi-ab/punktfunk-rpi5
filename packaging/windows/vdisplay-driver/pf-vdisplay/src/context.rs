use std::{
    mem::{self, size_of},
    num::{ParseIntError, TryFromIntError},
    ptr::{addr_of_mut, NonNull},
    sync::{Arc, Mutex},
};

use anyhow::anyhow;
use log::{error, info, warn};
use wdf_umdf::{
    IddCxAdapterInitAsync, IddCxError, IddCxMonitorArrival, IddCxMonitorCreate,
    IddCxMonitorSetupHardwareCursor, WdfError, WdfObjectDelete, WDF_DECLARE_CONTEXT_TYPE,
};
use wdf_umdf_sys::{
    DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY, HANDLE, IDARG_IN_ADAPTER_INIT, IDARG_IN_MONITORCREATE,
    IDARG_IN_SETUP_HWCURSOR, IDARG_OUT_ADAPTER_INIT, IDARG_OUT_MONITORARRIVAL,
    IDARG_OUT_MONITORCREATE, IDDCX_ADAPTER, IDDCX_ADAPTER_CAPS, IDDCX_ADAPTER_FLAGS, IDDCX_CURSOR_CAPS,
    IDDCX_ENDPOINT_DIAGNOSTIC_INFO, IDDCX_ENDPOINT_VERSION, IDDCX_FEATURE_IMPLEMENTATION,
    IDDCX_MONITOR, IDDCX_MONITOR_DESCRIPTION, IDDCX_MONITOR_DESCRIPTION_TYPE, IDDCX_MONITOR_INFO,
    IDDCX_SWAPCHAIN, IDDCX_TRANSMISSION_TYPE, IDDCX_XOR_CURSOR_SUPPORT, LUID, NTSTATUS, WDFDEVICE,
    WDFOBJECT, WDF_OBJECT_ATTRIBUTES,
};
use windows::{
    core::{s, w, GUID},
    Win32::{Foundation::TRUE, System::Threading::CreateEventA},
};

use crate::{
    direct_3d_device::Direct3DDevice,
    edid::Edid,
    monitor::MONITOR_MODES,
    swap_chain_processor::SwapChainProcessor,
};

// Maximum amount of monitors that can be connected
pub const MAX_MONITORS: u8 = 16;

/// ONE shared D3D render device, reused across every swap-chain assignment (keyed by render LUID).
/// Creating a fresh `Direct3DDevice` per assign — and the swap-chain flap fires several assigns per
/// session — spawned a new NVIDIA UMD worker-thread set each time that was NEVER reclaimed on release
/// (proven on the RTX box: ~70 `nvwgf2umx` threads + ~50 MB VRAM leaked per reconnect, permanently,
/// even though our `Direct3DDevice` refcount dropped to 0). Pooling one device keeps a single, stable
/// thread set: the processors borrow an `Arc`, so the device outlives them and is never re-created.
static DEVICE_POOL: Mutex<Option<(i64, Arc<Direct3DDevice>)>> = Mutex::new(None);

/// Get-or-create the pooled D3D device for `luid`. Re-creates only if the render adapter changes
/// (e.g. a GPU hot-swap), which drops the old `Arc` once its last processor releases it.
fn pooled_device(luid: windows::Win32::Foundation::LUID) -> Option<Arc<Direct3DDevice>> {
    let key = (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart as u32);
    let mut pool = DEVICE_POOL.lock().ok()?;
    if let Some((k, dev)) = pool.as_ref() {
        if *k == key {
            return Some(dev.clone());
        }
    }
    match Direct3DDevice::init(luid) {
        Ok(d) => {
            let a = Arc::new(d);
            *pool = Some((key, a.clone()));
            Some(a)
        }
        Err(e) => {
            error!("pooled Direct3DDevice::init failed: {e:?}");
            None
        }
    }
}

pub struct DeviceContext {
    device: WDFDEVICE,
    adapter: Option<IDDCX_ADAPTER>,
}

// SAFETY: Raw ptr is managed by external library
unsafe impl Send for DeviceContext {}
unsafe impl Sync for DeviceContext {}

// for now, `device` is hardcoded into the macro, so it needs to be there even if unused
#[allow(unused)]
pub struct MonitorContext {
    device: IDDCX_MONITOR,
    swap_chain_processor: Option<SwapChainProcessor>,
    /// OS target id (from IddCxMonitorArrival), stamped on this context at creation. assign_swap_chain
    /// uses THIS instead of a MONITOR_MODES pointer lookup — the lookup returns 0 for a recreated
    /// (session-2+) monitor, which broke the shared-ring naming and cascaded into SetDevice
    /// E_INVALIDARG + an access violation (the fix-teardown crash).
    target_id: u32,
}

// SAFETY: Raw ptr is managed by external library
unsafe impl Send for MonitorContext {}
unsafe impl Sync for MonitorContext {}

WDF_DECLARE_CONTEXT_TYPE!(pub DeviceContext);
WDF_DECLARE_CONTEXT_TYPE!(pub MonitorContext);

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("Failed to parse integer: {0:?}")]
    ParseInt(#[from] ParseIntError),
    #[error("Failed to convert integer: {0:?}")]
    TryFromInt(#[from] TryFromIntError),
    #[error("Failed to convert to NTSTATUS: {0:?}")]
    Ntstatus(#[from] NTSTATUS),
    #[error("Failed to convert to IddCxError: {0:?}")]
    IddCx(#[from] IddCxError),
    #[error("Failed to convert to WdfError: {0:?}")]
    Wdf(#[from] WdfError),
    #[error("Windows Error: {0:?}")]
    Win(#[from] windows::core::Error),
    #[error("{0:?}")]
    Other(#[from] anyhow::Error),
}

impl DeviceContext {
    pub fn new(device: WDFDEVICE) -> Self {
        Self {
            device,
            adapter: None,
        }
    }

    pub fn init_adapter(&mut self) -> Result<(), ContextError> {
        let mut version = IDDCX_ENDPOINT_VERSION {
            #[allow(clippy::cast_possible_truncation)]
            Size: size_of::<IDDCX_ENDPOINT_VERSION>() as u32,

            MajorVer: env!("CARGO_PKG_VERSION_MAJOR").parse::<u32>()?,
            MinorVer: env!("CARGO_PKG_VERSION_MINOR").parse::<u32>()?,
            Build: env!("CARGO_PKG_VERSION_PATCH").parse::<u32>()?,
            ..Default::default()
        };

        let mut adapter_caps = IDDCX_ADAPTER_CAPS {
            #[allow(clippy::cast_possible_truncation)]
            Size: size_of::<IDDCX_ADAPTER_CAPS>() as u32,

            // B2 HDR: declare we can process FP16 (scRGB) desktop surfaces — enables HDR10 / SDR WCG.
            // This OBLIGATES the *2 mode DDIs (done) + ReleaseAndAcquireBuffer2 (done in run_core).
            Flags: IDDCX_ADAPTER_FLAGS::IDDCX_ADAPTER_FLAGS_CAN_PROCESS_FP16,

            MaxMonitorsSupported: u32::from(MAX_MONITORS),

            EndPointDiagnostics: IDDCX_ENDPOINT_DIAGNOSTIC_INFO {
                #[allow(clippy::cast_possible_truncation)]
                Size: size_of::<IDDCX_ENDPOINT_DIAGNOSTIC_INFO>() as u32,
                GammaSupport: IDDCX_FEATURE_IMPLEMENTATION::IDDCX_FEATURE_IMPLEMENTATION_NONE,
                TransmissionType: IDDCX_TRANSMISSION_TYPE::IDDCX_TRANSMISSION_TYPE_WIRED_OTHER,

                pEndPointFriendlyName: w!("punktfunk Virtual Display Adapter").as_ptr(),
                pEndPointManufacturerName: w!("punktfunk").as_ptr(),
                pEndPointModelName: w!("Virtual Display").as_ptr(),

                pFirmwareVersion: addr_of_mut!(version).cast(),
                pHardwareVersion: addr_of_mut!(version).cast(),
            },

            ..Default::default()
        };

        let mut attr = WDF_OBJECT_ATTRIBUTES::init_context_type(unsafe { Self::get_type_info() });

        let adapter_init = IDARG_IN_ADAPTER_INIT {
            // this is WdfDevice because that's what we set last
            WdfDevice: self.device,
            pCaps: addr_of_mut!(adapter_caps).cast(),
            ObjectAttributes: addr_of_mut!(attr).cast(),
        };

        let mut adapter_init_out = IDARG_OUT_ADAPTER_INIT::default();
        unsafe { IddCxAdapterInitAsync(&adapter_init, &mut adapter_init_out)? };

        self.adapter = Some(adapter_init_out.AdapterObject);

        unsafe { self.clone_into(adapter_init_out.AdapterObject as WDFOBJECT)? };

        Ok(())
    }

    pub fn finish_init() -> NTSTATUS {
        // Monitors are created on demand by the IOCTL control plane (control::do_add). Start the
        // watchdog so a crashed/gone host never leaves a phantom display.
        crate::control::start_watchdog();
        NTSTATUS::STATUS_SUCCESS
    }

    pub fn create_monitor(&mut self, index: u32) -> Result<(), ContextError> {
        let mut attr =
            WDF_OBJECT_ATTRIBUTES::init_context_type(unsafe { MonitorContext::get_type_info() });

        // use the edid serial number to represent the monitor index for later identification
        let mut edid = Edid::generate_with(index);

        let mut monitor_info = IDDCX_MONITOR_INFO {
            #[allow(clippy::cast_possible_truncation)]
            Size: size_of::<IDDCX_MONITOR_INFO>() as u32,
            // SAFETY: windows-rs + generated _GUID types are same size, with same fields, and repr C
            // see: https://microsoft.github.io/windows-docs-rs/doc/windows/core/struct.GUID.html
            // and: wmdf_umdf_sys::_GUID
            MonitorContainerId: unsafe {
                mem::transmute::<GUID, wdf_umdf_sys::_GUID>(GUID::new()?)
            },
            MonitorType:
                DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY::DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,

            ConnectorIndex: index,
            MonitorDescription: IDDCX_MONITOR_DESCRIPTION {
                #[allow(clippy::cast_possible_truncation)]
                Size: size_of::<IDDCX_MONITOR_DESCRIPTION>() as u32,

                Type: IDDCX_MONITOR_DESCRIPTION_TYPE::IDDCX_MONITOR_DESCRIPTION_TYPE_EDID,

                #[allow(clippy::cast_possible_truncation)]
                DataSize: edid.len() as u32,

                pData: edid.as_mut_ptr().cast(),
            },
        };

        let monitor_create = IDARG_IN_MONITORCREATE {
            ObjectAttributes: &mut attr,
            pMonitorInfo: &mut monitor_info,
        };

        let mut monitor_create_out = IDARG_OUT_MONITORCREATE::default();
        unsafe {
            IddCxMonitorCreate(
                self.adapter.ok_or(anyhow!("Failed to get adapter"))?,
                &monitor_create,
                &mut monitor_create_out,
            )?
        };

        // store monitor object for later
        {
            let mut lock = MONITOR_MODES
                .lock()
                .map_err(|_| anyhow!("Failed to lock mutex"))?;

            for monitor in &mut *lock {
                if monitor.data.id == index {
                    monitor.object = Some(
                        NonNull::new(monitor_create_out.MonitorObject)
                            .ok_or(anyhow!("MonitorObject was null"))?,
                    );
                }
            }
        }

        unsafe {
            let context = MonitorContext::new(monitor_create_out.MonitorObject);
            context.init(monitor_create_out.MonitorObject as WDFOBJECT)?;
        }

        // tell os monitor is plugged in

        let mut arrival_out = IDARG_OUT_MONITORARRIVAL::default();

        unsafe {
            IddCxMonitorArrival(monitor_create_out.MonitorObject, &mut arrival_out)?;
        }

        // Record the OS target id + render-adapter LUID for the ADD IOCTL reply.
        {
            let mut lock = MONITOR_MODES
                .lock()
                .map_err(|_| anyhow!("Failed to lock mutex"))?;
            if let Some(mon) = lock.iter_mut().find(|m| m.data.id == index) {
                mon.target_id = arrival_out.OsTargetId;
                mon.adapter_luid_low = arrival_out.OsAdapterLuid.LowPart;
                mon.adapter_luid_high = arrival_out.OsAdapterLuid.HighPart;
            }
        }

        // Stamp the OS target id onto the monitor's CONTEXT so assign_swap_chain reads it directly
        // (no MONITOR_MODES pointer lookup, which returns 0 for a recreated monitor).
        unsafe {
            let _ = MonitorContext::get_mut(monitor_create_out.MonitorObject.cast(), |ctx| {
                ctx.target_id = arrival_out.OsTargetId;
            });
        }

        Ok(())
    }
}

impl MonitorContext {
    pub fn new(device: IDDCX_MONITOR) -> Self {
        Self {
            device,
            swap_chain_processor: None,
            target_id: 0,
        }
    }

    pub fn assign_swap_chain(
        &mut self,
        swap_chain: IDDCX_SWAPCHAIN,
        render_adapter: LUID,
        new_frame_event: HANDLE,
    ) {
        // drop processing thread
        drop(self.swap_chain_processor.take());

        // transmute would work, but one less unsafe block, so why not
        let luid = windows::Win32::Foundation::LUID {
            LowPart: render_adapter.LowPart,
            HighPart: render_adapter.HighPart,
        };

        // Log which GPU the OS picked to render this virtual monitor (useful on a hybrid iGPU+dGPU box,
        // where the render adapter determines which adapter the host's capture must enumerate).
        info!(
            "swap-chain assigned: OS render adapter LUID = {:08x}:{:08x}",
            render_adapter.HighPart, render_adapter.LowPart
        );

        // The OS target id keys the per-monitor shared frame-push objects (header/event/textures) the
        // host opens. Read it from THIS context (stamped at creation after IddCxMonitorArrival) — the
        // old MONITOR_MODES pointer lookup returned 0 for a recreated (session-2+) monitor, which broke
        // the ring naming and cascaded into SetDevice E_INVALIDARG + an access violation.
        let target_id = self.target_id;

        let device = pooled_device(luid);

        if let Some(device) = device {
            let mut processor = SwapChainProcessor::new();

            processor.run(
                swap_chain,
                device,
                new_frame_event,
                target_id,
                render_adapter.LowPart,
                render_adapter.HighPart,
            );

            self.swap_chain_processor = Some(processor);

            // Cursor is BAKED into the captured video: for IDD-push we deliberately do NOT advertise a
            // hardware cursor, so DWM software-composites the mouse cursor into the swapchain surface we
            // capture — the client then sees the cursor in the stream. (A future separate-plane cursor
            // would re-enable setup_hw_cursor + IddCxMonitorQueryHardwareCursor.) Not advertising one
            // also stops leaking a CreateEventA handle per assign.
        } else {
            // It's important to delete the swap-chain if D3D init fails, so the OS generates a fresh
            // swap-chain and retries.
            error!("pooled Direct3DDevice unavailable for render LUID — deleting swap chain for OS retry");

            unsafe {
                let _ = WdfObjectDelete(swap_chain.cast());
            }
        }
    }

    pub fn unassign_swap_chain(&mut self) {
        let had = self.swap_chain_processor.take().is_some();
        error!("unassign_swap_chain (target={}) — dropped live processor: {had}", self.target_id);
    }

    /// Advertise a HARDWARE cursor. NOT called for IDD-push — we bake the cursor into the video
    /// instead (see `assign_swap_chain`). Kept for a future separate-plane cursor (which would pair it
    /// with `IddCxMonitorQueryHardwareCursor`). Leaks a `CreateEventA` handle per call, so only wire it
    /// back up alongside a real cursor-plane consumer.
    #[allow(dead_code)]
    pub fn setup_hw_cursor(&mut self) {
        let mouse_event = unsafe { CreateEventA(None, false, false, s!("vdd_mouse_event")) };
        let Ok(mouse_event) = mouse_event else {
            error!("CreateEventA failed: {mouse_event:?}");
            return;
        };

        // setup hardware cursor
        let cursor_info = IDDCX_CURSOR_CAPS {
            #[allow(clippy::cast_possible_truncation)]
            Size: std::mem::size_of::<IDDCX_CURSOR_CAPS>() as u32,
            AlphaCursorSupport: TRUE.0,
            MaxX: 512,
            MaxY: 512,
            ColorXorCursorSupport: IDDCX_XOR_CURSOR_SUPPORT::IDDCX_XOR_CURSOR_SUPPORT_NONE,
        };

        let hw_cursor = IDARG_IN_SETUP_HWCURSOR {
            CursorInfo: cursor_info,
            hNewCursorDataAvailable: mouse_event.0,
        };

        let res = unsafe { IddCxMonitorSetupHardwareCursor(self.device, &hw_cursor) };
        let Ok(res) = res else {
            error!("IddCxMonitorSetupHardwareCursor() failed: {res:?}");
            return;
        };

        if res.is_warning() {
            warn!("IddCxMonitorSetupHardwareCursor() warn: {res:?}");
        }
        if res.is_error() {
            error!("IddCxMonitorSetupHardwareCursor() failed: {res:?}");
        }
    }
}
