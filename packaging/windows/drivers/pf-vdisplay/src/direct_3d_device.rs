//! The render-side D3D11 device the swap-chain processor binds to the IddCx swap-chain (STEP 5).
//!
//! Ported verbatim from the proven oracle (`packaging/windows/vdisplay-driver/pf-vdisplay/src/
//! direct_3d_device.rs` + the `DEVICE_POOL`/`pooled_device` that lived in its `context.rs`). The
//! D3D/DXGI types are the `windows` crate (refcounted COM, no manual Drop); the swap-chain/LUID hand-off
//! to the wdk-sys IddCx world happens via raw pointers in `swap_chain_processor.rs`.
//!
//! STEP 5 binds this device to the swap-chain to keep the monitor a live display; STEP 6 reuses the
//! device's immediate context in the frame publisher's `CopyResource` on each swap-chain processor
//! thread. The device is POOLED across processors (one per render LUID, [`pooled_device`]), so with
//! two live monitors two worker threads share it concurrently — creation must NOT pass
//! `D3D11_CREATE_DEVICE_SINGLETHREADED` (that was sound only pre-pooling, device-per-processor), and
//! the immediate context is `SetMultithreadProtected` (it has no internal locking of its own).

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use windows::{
    Win32::{
        Foundation::{BOOL, LUID},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_PREVENT_ALTERING_LAYER_SETTINGS_FROM_REGISTRY,
                D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                ID3D11Multithread,
            },
            Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory5},
        },
    },
    core::{Error, Interface},
};

#[derive(thiserror::Error, Debug)]
pub enum Direct3DError {
    #[error("Direct3DError({0:?})")]
    Win32(#[from] Error),
    #[error("Direct3DError(\"{0}\")")]
    Other(&'static str),
}

impl From<&'static str> for Direct3DError {
    fn from(value: &'static str) -> Self {
        Direct3DError::Other(value)
    }
}

/// DIAGNOSTIC: live `Direct3DDevice` count. Each one holds an `ID3D11Device` whose NVIDIA UMD spawns
/// ~dozens of worker threads; if this climbs without bound across reconnects, devices are leaking.
pub static LIVE_DEVICES: AtomicI32 = AtomicI32::new(0);

#[derive(Debug)]
pub struct Direct3DDevice {
    // The following are already refcounted, so they're safe to use directly without additional drop impls
    _dxgi_factory: IDXGIFactory5,
    _adapter: IDXGIAdapter1,
    pub device: ID3D11Device,
    /// The shared immediate context — used by STEP 6's frame-push publisher's `CopyResource` on each
    /// swap-chain processor thread. Pooled across processors, so it is `SetMultithreadProtected` at
    /// init: an immediate context has no internal locking, and two concurrent monitors' workers would
    /// otherwise race it (undefined behavior inside the UMD).
    pub device_context: ID3D11DeviceContext,
}

impl Direct3DDevice {
    pub fn init(adapter_luid: LUID) -> Result<Self, Direct3DError> {
        // SAFETY: a plain DXGI factory-creation call; `?` returns the error on failure.
        let dxgi_factory =
            unsafe { CreateDXGIFactory2::<IDXGIFactory5>(DXGI_CREATE_FACTORY_FLAGS(0))? };

        // SAFETY: `dxgi_factory` is the live factory just created; `adapter_luid` is a by-value LUID.
        let adapter = unsafe { dxgi_factory.EnumAdapterByLuid::<IDXGIAdapter1>(adapter_luid)? };

        let mut device = None;
        let mut device_context = None;

        // SAFETY: `adapter` is a live IDXGIAdapter1; `device`/`device_context` are valid local out-params
        // (checked for None below); the flag set + SDK version are valid constants. `?` returns on failure.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                // NO `D3D11_CREATE_DEVICE_SINGLETHREADED`: the DEVICE_POOL shares this device (and
                // its immediate context) across every swap-chain processor on the LUID, so the
                // single-caller guarantee that flag declares no longer holds with >1 monitor.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT
                    | D3D11_CREATE_DEVICE_PREVENT_ALTERING_LAYER_SETTINGS_FROM_REGISTRY,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut device_context),
            )?;
        }

        let device = device.ok_or("ID3D11Device not found")?;
        let device_context = device_context.ok_or("ID3D11DeviceContext not found")?;

        // The pool hands this device (and its immediate context) to every processor on the LUID, and
        // an immediate context is not thread-safe by itself — turn on the runtime's per-call critical
        // section. (D3D11.4 interface, guaranteed on the Win11-22H2 OS floor; if the cast ever fails
        // we log and continue — a single monitor is still safe, concurrent ones would not be.)
        match device_context.cast::<ID3D11Multithread>() {
            Ok(mt) => {
                // SAFETY: plain setter on the live context's multithread interface; the returned
                // previous-state BOOL carries no obligation.
                unsafe {
                    let _ = mt.SetMultithreadProtected(BOOL::from(true));
                }
            }
            Err(e) => dbglog!(
                "[pf-vd] ID3D11Multithread unavailable ({e:?}) — immediate context left unprotected"
            ),
        }

        let live = LIVE_DEVICES.fetch_add(1, Ordering::Relaxed) + 1;
        dbglog!("[pf-vd] Direct3DDevice::init OK — live D3D devices = {live}");

        Ok(Self {
            _dxgi_factory: dxgi_factory,
            _adapter: adapter,
            device,
            device_context,
        })
    }
}

impl Drop for Direct3DDevice {
    fn drop(&mut self) {
        let live = LIVE_DEVICES.fetch_sub(1, Ordering::Relaxed) - 1;
        dbglog!("[pf-vd] Direct3DDevice::drop — live D3D devices = {live}");
    }
}

/// ONE shared D3D render device, reused across every swap-chain assignment (keyed by render LUID).
/// Creating a fresh `Direct3DDevice` per assign — and the swap-chain flap fires several assigns per
/// session — spawned a new NVIDIA UMD worker-thread set each time that was NEVER reclaimed on release
/// (proven on the RTX box: ~70 `nvwgf2umx` threads + ~50 MB VRAM leaked per reconnect, permanently,
/// even though our `Direct3DDevice` refcount dropped to 0). Pooling one device keeps a single, stable
/// thread set: the processors borrow an `Arc`, so the device outlives them and is never re-created.
static DEVICE_POOL: Mutex<Option<(i64, Arc<Direct3DDevice>)>> = Mutex::new(None);

/// Get-or-create the pooled D3D device for `luid`. Re-creates only if the render adapter changes
/// (e.g. a GPU hot-swap), which drops the old `Arc` once its last processor releases it.
pub fn pooled_device(luid: LUID) -> Option<Arc<Direct3DDevice>> {
    let key = (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart);
    let mut pool = DEVICE_POOL.lock().ok()?;
    if let Some((k, dev)) = pool.as_ref()
        && *k == key
    {
        // A TDR / driver reset REMOVES the pooled device permanently; handing it out again gives
        // every future swap-chain a dead device (SetDevice fail-loop → black virtual display until
        // device teardown). Detect and fall through to a fresh create instead.
        // SAFETY: plain status query on the live pooled device.
        match unsafe { dev.device.GetDeviceRemovedReason() } {
            Ok(()) => return Some(dev.clone()),
            Err(e) => {
                dbglog!("[pf-vd] pooled D3D device was REMOVED ({e:?}) — recreating on {key:#x}");
            }
        }
    }
    match Direct3DDevice::init(luid) {
        Ok(d) => {
            let a = Arc::new(d);
            *pool = Some((key, a.clone()));
            Some(a)
        }
        Err(e) => {
            dbglog!("[pf-vd] pooled Direct3DDevice::init failed: {e:?}");
            None
        }
    }
}
