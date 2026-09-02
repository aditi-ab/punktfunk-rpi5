//! Unhandled-SEH filter: one `tracing` ERROR, then default handling.
//!
//! Native crashes leave no Rust panic and no log line. This filter names the exception code,
//! fault address, and containing module before the process dies. The panic analogue lives in
//! `main()`.
//!
//! Call [`install`] once, after logging init — earlier logs into the void. The filter allocates;
//! heap corruption can fault again, and the OS then terminates as it would have. Always returns
//! `EXCEPTION_CONTINUE_SEARCH` so WER, a debugger, or the service supervisor still run.

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::{
    SetUnhandledExceptionFilter, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};

/// After logging init: the filter reports through `tracing`.
pub fn install() {
    // SAFETY: `on_unhandled` is `extern "system"`, matches LPTOP_LEVEL_EXCEPTION_FILTER, and
    // has static lifetime. The previous filter is dropped: this crate is the only installer.
    unsafe {
        SetUnhandledExceptionFilter(Some(on_unhandled));
    }
}

/// Slots 0-1 are access kind (0 read / 1 write / 8 execute) and the fault address; those
/// distinguish a wild pointer from a guard page.
const STATUS_ACCESS_VIOLATION: i32 = 0xC0000005u32 as i32;

/// Best-effort: formats and logs, so heap corruption can fault again. Returns
/// `EXCEPTION_CONTINUE_SEARCH` so WER / a debugger / the supervisor still run.
unsafe extern "system" fn on_unhandled(info: *const EXCEPTION_POINTERS) -> i32 {
    let mut code: i32 = 0;
    let mut addr: usize = 0;
    let mut av_kind: Option<usize> = None;
    let mut av_target: Option<usize> = None;
    // SAFETY: `info` (and `ExceptionRecord`) are supplied by the OS for the duration of this
    // callback; both are checked non-null before the read, and only plain fields are copied out.
    unsafe {
        if !info.is_null() && !(*info).ExceptionRecord.is_null() {
            let r = &*(*info).ExceptionRecord;
            code = r.ExceptionCode.0;
            addr = r.ExceptionAddress as usize;
            if code == STATUS_ACCESS_VIOLATION && r.NumberParameters >= 2 {
                av_kind = Some(r.ExceptionInformation[0]);
                av_target = Some(r.ExceptionInformation[1]);
            }
        }
    }
    let module = module_at(addr);
    tracing::error!(
        code = %format!("0x{:08x}", code as u32),
        address = %format!("0x{addr:016x}"),
        module = %module.as_deref().unwrap_or("<unknown>"),
        av_kind = av_kind.map(|k| match k {
            0 => "read",
            1 => "write",
            8 => "execute",
            _ => "other",
        }),
        av_target = av_target.map(|t| format!("0x{t:016x}")),
        "FATAL: unhandled native exception — the host process is about to die"
    );
    EXCEPTION_CONTINUE_SEARCH
}

/// So the log names the faulting DLL rather than a raw address.
fn module_at(addr: usize) -> Option<String> {
    if addr == 0 {
        return None;
    }
    let mut hmod = HMODULE::default();
    // SAFETY: FROM_ADDRESS treats the "module name" argument as an address inside the module
    // (`addr as *const u16`). UNCHANGED_REFCOUNT skips AddRef, so this HMODULE is not Freed.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(addr as *const u16),
            &mut hmod,
        )
        .ok()?;
    }
    let mut buf = [0u16; 512];
    // SAFETY: `hmod` is the handle from GetModuleHandleExW above; `buf` is a live writable
    // slice for the call.
    let n = unsafe { GetModuleFileNameW(Some(hmod), &mut buf) } as usize;
    (n > 0).then(|| String::from_utf16_lossy(&buf[..n.min(buf.len())]))
}
