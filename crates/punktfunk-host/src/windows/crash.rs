//! Last-resort crash visibility (Windows): an unhandled-SEH filter that logs a native crash —
//! an access violation inside a driver/runtime DLL (amfrt64, the GPU user-mode driver, d3d11,
//! IddCx plumbing) — through `tracing` before the process dies.
//!
//! Motivated by the "host went Offline with zero errors in the logs" class of field report: a
//! native crash kills the process (dropping the mDNS advert → every client tile flips Offline)
//! while the log ring's last entry is a healthy INFO line, so the report arrives with no evidence
//! at all. One ERROR line naming the exception code and the faulting module turns that into a
//! diagnosis. The Rust-panic analogue (a panic hook that tees into `tracing`) lives in `main()`.

// Every `unsafe` block in this file carries a `// SAFETY:` proof (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::{
    SetUnhandledExceptionFilter, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};

/// Install the process-wide unhandled-exception filter. Call once at startup, after logging init
/// (the filter reports through `tracing`, so installing it earlier would log into the void).
pub fn install() {
    // SAFETY: registers a process-wide top-level exception filter; `on_unhandled` is a plain
    // `extern "system"` fn with the LPTOP_LEVEL_EXCEPTION_FILTER signature and static lifetime.
    // The returned previous filter is deliberately dropped — we are the first/only installer.
    unsafe {
        SetUnhandledExceptionFilter(Some(on_unhandled));
    }
}

/// STATUS_ACCESS_VIOLATION — the overwhelmingly common native-crash code; its first two
/// `ExceptionInformation` slots carry the access kind (0 read / 1 write / 8 execute) and the
/// target address, which we surface because they distinguish a wild pointer from a guard page.
const STATUS_ACCESS_VIOLATION: i32 = 0xC0000005u32 as i32;

/// The filter itself. Best-effort by design: it formats and logs, which allocates — if the crash
/// is heap corruption this can fault again, in which case the OS terminates us exactly as it was
/// about to anyway. Returns `EXCEPTION_CONTINUE_SEARCH` so default handling (WER / a debugger /
/// the service supervisor seeing the exit) still runs.
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

/// Resolve the module (DLL/EXE) containing `addr` — the smoking gun that separates "our bug" from
/// "the GPU runtime crashed under us" (amfrt64.dll, atiumd64.dll, nvwgf2umx.dll, d3d11.dll, …).
fn module_at(addr: usize) -> Option<String> {
    if addr == 0 {
        return None;
    }
    let mut hmod = HMODULE::default();
    // SAFETY: FROM_ADDRESS reinterprets the "module name" parameter as an address inside the
    // module — `addr as *const u16` is exactly that; UNCHANGED_REFCOUNT means no AddRef, so the
    // returned HMODULE needs no FreeLibrary. `&mut hmod` is a live out-pointer for the call.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(addr as *const u16),
            &mut hmod,
        )
        .ok()?;
    }
    let mut buf = [0u16; 512];
    // SAFETY: `hmod` is the module handle resolved above; `buf` is a live, writable slice for the
    // duration of the call. Returns the number of UTF-16 units written (0 on failure).
    let n = unsafe { GetModuleFileNameW(Some(hmod), &mut buf) } as usize;
    (n > 0).then(|| String::from_utf16_lossy(&buf[..n.min(buf.len())]))
}
