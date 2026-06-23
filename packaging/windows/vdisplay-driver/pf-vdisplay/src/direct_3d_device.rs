use std::sync::atomic::{AtomicI32, Ordering};

use windows::{
    core::Error,
    Win32::{
        Foundation::LUID,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_PREVENT_ALTERING_LAYER_SETTINGS_FROM_REGISTRY,
                D3D11_CREATE_DEVICE_SINGLETHREADED, D3D11_SDK_VERSION,
            },
            Dxgi::{CreateDXGIFactory2, IDXGIAdapter1, IDXGIFactory5, DXGI_CREATE_FACTORY_FLAGS},
        },
    },
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
    /// The single (SINGLETHREADED) immediate context — used by the frame-push publisher's
    /// `CopyResource` on the swap-chain processor thread (the one thread this device is touched from).
    pub device_context: ID3D11DeviceContext,
}

impl Direct3DDevice {
    pub fn init(adapter_luid: LUID) -> Result<Self, Direct3DError> {
        let dxgi_factory =
            unsafe { CreateDXGIFactory2::<IDXGIFactory5>(DXGI_CREATE_FACTORY_FLAGS(0))? };

        let adapter = unsafe { dxgi_factory.EnumAdapterByLuid::<IDXGIAdapter1>(adapter_luid)? };

        let mut device = None;
        let mut device_context = None;

        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT
                    | D3D11_CREATE_DEVICE_SINGLETHREADED
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

        let live = LIVE_DEVICES.fetch_add(1, Ordering::Relaxed) + 1;
        log::error!("Direct3DDevice::init OK — live D3D devices = {live}");

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
        log::error!("Direct3DDevice::drop — live D3D devices = {live}");
    }
}
