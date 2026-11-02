//! `pf_seat_audio.dll`: the audio endpoint enumerator of punktfunk's seat listener.
//!
//! Remote Desktop names it in the `punktfunk-tcp` listener's `AudioEnumeratorDll`. Windows loads
//! it into AudioEndpointBuilder, Audiosrv, audiodg and every process of a seat session that uses
//! MMDevAPI. It returns the stock `rdpendp` enumerator's endpoints plus a DualSense render
//! endpoint for each pad slot the session's host owns (`pf_driver_proto::seat_audio`). Any
//! failure hands back the stock enumerator, so the seat keeps Remote Audio.
//!
//! Code here runs inside Windows' audio services and other people's processes: nothing may
//! panic across a COM boundary, nothing on the engine's real-time path may block, and no `Drop`
//! joins a thread — at process exit the thread is already gone and `join` panics.
#![cfg(windows)]

mod endpoint;
mod enumerator;

use core::ffi::c_void;
use std::sync::OnceLock;
use windows::core::{s, w, Interface, HRESULT};
use windows::Win32::Foundation::{E_POINTER, E_UNEXPECTED, HMODULE, S_OK};
use windows::Win32::Media::Audio::IMMDeviceEnumerator;
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GetProcAddress, LoadLibraryExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_PIN, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::GetCurrentProcessId;

type GetEnumerator = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;

/// Remote Desktop's hook. `session` is the session's id, or `u32::MAX` from a process inside it.
///
/// # Safety
/// `out` is null or valid for one interface pointer.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn GetTSAudioEndpointEnumeratorForSession(
    session: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let made = std::panic::catch_unwind(|| enumerator_for(session));
    let (raw, hr) = match made {
        Ok(Ok(enumerator)) => (enumerator.into_raw(), S_OK),
        Ok(Err(error)) => (core::ptr::null_mut(), error.code()),
        Err(_) => (core::ptr::null_mut(), E_UNEXPECTED),
    };
    // SAFETY: `out` is non-null and the caller's slot for one interface pointer.
    unsafe { *out = raw };
    hr
}

/// The stock enumerator, wrapped with the session's pad endpoints when that works.
fn enumerator_for(session: u32) -> windows::core::Result<IMMDeviceEnumerator> {
    pin_module();
    let base = stock(session)?;
    let session = if session == u32::MAX {
        own_session()
    } else {
        Some(session)
    };
    Ok(session
        .and_then(|session| enumerator::wrap(session, base.clone()).ok())
        .unwrap_or(base))
}

/// `rdpendp.dll`'s enumerator for `session`, at this process's bitness.
fn stock(session: u32) -> windows::core::Result<IMMDeviceEnumerator> {
    static EXPORT: OnceLock<Option<GetEnumerator>> = OnceLock::new();
    let export = EXPORT.get_or_init(|| {
        // SAFETY: loads a System32 library by name. It stays loaded for the process, which
        // the stock objects handed out below rely on.
        let module =
            unsafe { LoadLibraryExW(w!("rdpendp.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
                .ok()?;
        // SAFETY: `module` is loaded; the name is a static C string.
        let proc = unsafe { GetProcAddress(module, s!("GetTSAudioEndpointEnumeratorForSession")) }?;
        // SAFETY: the export has Remote Desktop's documented hook signature.
        Some(unsafe {
            core::mem::transmute::<unsafe extern "system" fn() -> isize, GetEnumerator>(proc)
        })
    });
    let Some(export) = export else {
        return Err(E_UNEXPECTED.into());
    };
    let mut raw = core::ptr::null_mut();
    // SAFETY: the stock hook with a session id and a slot for one interface pointer.
    unsafe { export(session, &mut raw) }.ok()?;
    if raw.is_null() {
        return Err(E_UNEXPECTED.into());
    }
    // SAFETY: on success the hook returned one owned `IMMDeviceEnumerator` reference.
    Ok(unsafe { IMMDeviceEnumerator::from_raw(raw) })
}

fn own_session() -> Option<u32> {
    let mut session = 0;
    // SAFETY: plain query of this process.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }.ok()?;
    Some(session)
}

/// Keep this DLL mapped for the process's life: its objects and watcher threads outlive any
/// `FreeLibrary` MMDevAPI might issue.
fn pin_module() {
    static PINNED: OnceLock<()> = OnceLock::new();
    PINNED.get_or_init(|| {
        let mut module = HMODULE::default();
        // SAFETY: the address is a function in this module; the handle is not used afterwards.
        let _ = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                windows::core::PCWSTR(pin_module as *const u16),
                &mut module,
            )
        };
    });
}
