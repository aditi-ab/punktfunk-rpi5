//! Host-side WGC helper relay (Windows two-process secure-desktop design,
//! docs/windows-secure-desktop.md — step 4).
//!
//! WGC won't activate under the SYSTEM account, so the SYSTEM host can't capture the normal desktop
//! itself. Instead it spawns `m3-host wgc-helper` in the **interactive user session** (so WGC works)
//! via `CreateProcessAsUserW`, with the helper's **stdout** redirected to an anonymous pipe the host
//! reads. The helper ships framed Annex-B access units; this module parses them back into AUs the
//! host relays onto the live QUIC session (same `EncodedFrame` flow, just sourced over a pipe instead
//! of a local encoder). A second pipe carries a tiny **control** channel to the helper (stdin: force
//! keyframe), and the helper's **stderr** is forwarded line-by-line into host tracing so its logs are
//! visible from the SYSTEM host's console.
//!
//! Wire framing (must match `wgc_helper::write_au`): per AU
//! `[u32 magic "PFAU" LE][u32 len LE][u64 pts_ns LE][u8 keyframe][len bytes data]`.

use crate::capture::dxgi::WinCaptureTarget;
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Mutex;
use windows::core::PWSTR;
use windows::Win32::Foundation::SetHandleInformation;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Foundation::{HANDLE_FLAGS, HANDLE_FLAG_INHERIT};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TokenPrimary, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, TerminateProcess, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

/// Must match [`crate::wgc_helper`]'s `AU_MAGIC` ("PFAU").
const AU_MAGIC: u32 = 0x5046_4155;

/// One access unit relayed from the helper, in the helper's (= the host's, same machine) monotonic
/// clock — `pts_ns` is directly comparable to the host's `now_ns()`.
pub struct RelayAu {
    pub data: Vec<u8>,
    pub pts_ns: u64,
    pub keyframe: bool,
}

/// A running USER-session WGC helper whose AUs the SYSTEM host relays. Drop kills the child + closes
/// the pipes; the reader threads then end on the broken pipe.
pub struct HelperRelay {
    proc: HANDLE,
    thread: HANDLE,
    /// Host write end of the helper's stdin — control commands (force keyframe). Mutex so the relay
    /// can be shared while the encode thread requests keyframes.
    stdin_w: Mutex<HANDLE>,
    /// Parsed AUs from the helper's stdout reader thread.
    rx: Receiver<RelayAu>,
}

// HANDLEs are just kernel handle values; we own them for the relay's lifetime and close them on Drop.
unsafe impl Send for HelperRelay {}
unsafe impl Sync for HelperRelay {}

/// Control byte on the helper's stdin: force the next encoded frame to be an IDR (client decode
/// recovery). Mirrors `enc.request_keyframe()` in the single-process path.
const CTL_KEYFRAME: u8 = 0x01;

impl HelperRelay {
    /// Spawn the helper in the interactive user session and start relaying its AUs. `target` is the
    /// SudoVDA output the host already created (captured by GDI name only — the helper never touches
    /// display topology). `(w, h, hz)` is the negotiated mode; `bitrate_kbps` the negotiated bitrate.
    pub fn spawn(
        target: &WinCaptureTarget,
        mode: (u32, u32, u32),
        bitrate_kbps: u32,
        bit_depth: u8,
    ) -> Result<HelperRelay> {
        let exe = std::env::current_exe().context("current_exe for helper spawn")?;
        let exe = exe.to_string_lossy().into_owned();
        let (w, h, hz) = mode;
        // CreateProcessAsUserW takes a single mutable command line (argv[0] = exe).
        let cmdline = format!(
            "\"{exe}\" wgc-helper --gdi \"{}\" --target-id {} --mode {w}x{h}x{hz} --bitrate {bitrate_kbps} --bit-depth {bit_depth}",
            target.gdi_name, target.target_id
        );
        tracing::info!(cmd = %cmdline, "spawning WGC helper in user session");

        unsafe { spawn_inner(&cmdline, w, h, hz) }
    }

    /// Receive the next relayed AU. Distinguishes a `Timeout` (helper slow/stalled — keep waiting)
    /// from `Disconnected` (helper exited → its stdout closed → reader thread ended → channel
    /// dropped), which returns *immediately* and means the relay must stop, not spin.
    pub fn recv_timeout(
        &self,
        dur: std::time::Duration,
    ) -> Result<RelayAu, std::sync::mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(dur)
    }

    /// Non-blocking receive — used to drain stale buffered AUs (encoded while the secure desktop was
    /// the live source) before resuming the relay. `Ok` while AUs remain, `Err` once empty.
    pub fn try_recv(&self) -> Result<RelayAu, std::sync::mpsc::TryRecvError> {
        self.rx.try_recv()
    }

    /// Ask the helper's encoder for an IDR on the next frame (client decode recovery). Best-effort:
    /// a write failure means the helper is gone — the caller's recv loop will see the disconnect.
    pub fn request_keyframe(&self) {
        let h = self.stdin_w.lock().unwrap();
        let mut written = 0u32;
        unsafe {
            let _ = windows::Win32::Storage::FileSystem::WriteFile(
                *h,
                Some(&[CTL_KEYFRAME]),
                Some(&mut written),
                None,
            );
        }
    }
}

impl Drop for HelperRelay {
    fn drop(&mut self) {
        unsafe {
            // Terminate the child first so its WGC capture + NVENC session tear down, then close our
            // handles (the reader threads end on the resulting broken pipe).
            let _ = TerminateProcess(self.proc, 1);
            let _ = CloseHandle(*self.stdin_w.lock().unwrap());
            let _ = CloseHandle(self.proc);
            let _ = CloseHandle(self.thread);
        }
        tracing::info!("WGC helper relay torn down");
    }
}

/// Inheritable anonymous pipe (read, write). The caller marks whichever end the host keeps as
/// non-inheritable so the child only inherits its own end.
unsafe fn make_pipe() -> Result<(HANDLE, HANDLE)> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    CreatePipe(&mut read, &mut write, Some(&sa), 0).context("CreatePipe")?;
    Ok((read, write))
}

/// Mark a handle non-inheritable (the host keeps it; the child must not get a copy).
unsafe fn no_inherit(h: HANDLE) {
    let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
}

/// Build the helper's environment block: the user's block (so DLL/PATH/SystemRoot resolve) with this
/// (host) process's `PUNKTFUNK_*` vars overlaid, so the helper encodes with the SAME settings the
/// host runs with (`PUNKTFUNK_ENCODER=nvenc`, `PUNKTFUNK_ZEROCOPY`, …) instead of the user shell's.
/// Returns a UTF-16, double-null-terminated block suitable for `CREATE_UNICODE_ENVIRONMENT`.
unsafe fn merged_env_block(user_block: *const u16) -> Vec<u16> {
    // Parse the user block ("VAR=VALUE\0" … "\0") into entries.
    let mut entries: Vec<String> = Vec::new();
    if !user_block.is_null() {
        let mut p = user_block;
        loop {
            let mut len = 0isize;
            while *p.offset(len) != 0 {
                len += 1;
            }
            if len == 0 {
                break; // the trailing empty string = end of block
            }
            let slice = std::slice::from_raw_parts(p, len as usize);
            entries.push(String::from_utf16_lossy(slice));
            p = p.offset(len + 1);
        }
    }
    // Drop any PUNKTFUNK_* the user block carried, then overlay this process's PUNKTFUNK_* vars.
    entries.retain(|e| !e.split('=').next().unwrap_or("").starts_with("PUNKTFUNK_"));
    for (k, v) in std::env::vars().filter(|(k, _)| k.starts_with("PUNKTFUNK_")) {
        entries.push(format!("{k}={v}"));
    }
    // Serialize back to a UTF-16 double-null-terminated block.
    let mut block: Vec<u16> = Vec::new();
    for e in entries {
        block.extend(e.encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

unsafe fn spawn_inner(cmdline: &str, w: u32, h: u32, hz: u32) -> Result<HelperRelay> {
    // The user token of the active console session (requires the host to be SYSTEM).
    let session = WTSGetActiveConsoleSessionId();
    if session == 0xFFFF_FFFF {
        bail!("no active console session (WTSGetActiveConsoleSessionId)");
    }
    let mut user_token = HANDLE::default();
    WTSQueryUserToken(session, &mut user_token)
        .context("WTSQueryUserToken (host must run as SYSTEM)")?;

    // A primary token for CreateProcessAsUserW.
    let mut primary = HANDLE::default();
    let dup = DuplicateTokenEx(
        user_token,
        TOKEN_ALL_ACCESS,
        None,
        SecurityImpersonation,
        TokenPrimary,
        &mut primary,
    );
    let _ = CloseHandle(user_token);
    dup.context("DuplicateTokenEx(TokenPrimary)")?;

    // The user's environment block (PATH, USERPROFILE, SystemRoot → DLL resolution), MERGED with the
    // host's PUNKTFUNK_* vars. CreateProcessAsUserW would otherwise give the helper the *user's* env
    // only, dropping PUNKTFUNK_ENCODER=nvenc / PUNKTFUNK_ZEROCOPY/… that the host runs with — so the
    // helper would fall back to the software (H.264-only) encoder. We parse the user block, strip any
    // PUNKTFUNK_* it has, append the host's, and pass the merged block.
    let mut env_block: *mut core::ffi::c_void = std::ptr::null_mut();
    let _ = CreateEnvironmentBlock(&mut env_block, Some(primary), false);
    let merged_env = merged_env_block(env_block as *const u16);
    if !env_block.is_null() {
        let _ = DestroyEnvironmentBlock(env_block);
    }

    // Three pipes: stdout (helper→host AUs), stdin (host→helper control), stderr (helper→host logs).
    let (out_r, out_w) = make_pipe().context("stdout pipe")?;
    let (in_r, in_w) = make_pipe().context("stdin pipe")?;
    let (err_r, err_w) = make_pipe().context("stderr pipe")?;
    // The host keeps out_r / in_w / err_r — none inheritable; the child inherits out_w/in_r/err_w.
    no_inherit(out_r);
    no_inherit(in_w);
    no_inherit(err_r);

    let mut si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        hStdInput: in_r,
        hStdOutput: out_w,
        hStdError: err_w,
        ..Default::default()
    };
    // WGC needs the interactive desktop.
    let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
    si.lpDesktop = PWSTR(desktop.as_mut_ptr());

    let mut cmd: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
    let mut pi = PROCESS_INFORMATION::default();

    let created = CreateProcessAsUserW(
        Some(primary),
        None,
        Some(PWSTR(cmd.as_mut_ptr())),
        None,
        None,
        true, // inherit handles (the child's std ends)
        CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
        Some(merged_env.as_ptr() as *const core::ffi::c_void),
        None,
        &si,
        &mut pi,
    );

    // Clean up regardless of outcome: the child now owns its inherited ends; close our copies.
    let _ = CloseHandle(out_w);
    let _ = CloseHandle(in_r);
    let _ = CloseHandle(err_w);
    let _ = CloseHandle(primary);

    if let Err(e) = created {
        let _ = CloseHandle(out_r);
        let _ = CloseHandle(in_w);
        let _ = CloseHandle(err_r);
        return Err(e).context("CreateProcessAsUserW(wgc-helper)");
    }
    tracing::info!(pid = pi.dwProcessId, mode = %format!("{w}x{h}@{hz}"), "WGC helper spawned");

    // stderr → host tracing, line by line.
    let err_handle = HandleReader(err_r);
    std::thread::Builder::new()
        .name("wgc-helper-log".into())
        .spawn(move || {
            let r = BufReader::new(err_handle);
            for line in r.lines() {
                match line {
                    Ok(l) if !l.trim().is_empty() => tracing::info!(target: "wgc_helper", "{l}"),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        })
        .ok();

    // stdout → parsed AUs. Bounded so a stalled relay applies backpressure (the pipe then fills and
    // the helper blocks on write — the same backpressure the single-process channel gives).
    let (tx, rx) = std::sync::mpsc::sync_channel::<RelayAu>(3);
    let out_handle = HandleReader(out_r);
    std::thread::Builder::new()
        .name("wgc-helper-au".into())
        .spawn(move || au_reader(out_handle, tx))
        .ok();

    Ok(HelperRelay {
        proc: pi.hProcess,
        thread: pi.hThread,
        stdin_w: Mutex::new(in_w),
        rx,
    })
}

/// Parse the AU framing off the helper's stdout and forward each AU. Ends (returns) when the pipe
/// breaks (helper exit) or the channel's receiver is dropped (relay torn down).
fn au_reader(mut r: HandleReader, tx: SyncSender<RelayAu>) {
    loop {
        let mut hdr = [0u8; 4 + 4 + 8 + 1];
        if r.read_exact(&mut hdr).is_err() {
            break;
        }
        let magic = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        if magic != AU_MAGIC {
            tracing::error!(
                magic = format!("{magic:#x}"),
                "WGC helper AU stream desync — aborting relay"
            );
            break;
        }
        let len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
        let pts_ns = u64::from_le_bytes([
            hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
        ]);
        let keyframe = hdr[16] != 0;
        // Bound the allocation — a corrupt length must not OOM the host. 64 MiB is far above any real
        // AU (a 5K keyframe is a few MB).
        if len > 64 * 1024 * 1024 {
            tracing::error!(len, "WGC helper AU length implausible — aborting relay");
            break;
        }
        let mut data = vec![0u8; len];
        if r.read_exact(&mut data).is_err() {
            break;
        }
        if tx
            .send(RelayAu {
                data,
                pts_ns,
                keyframe,
            })
            .is_err()
        {
            break; // relay dropped
        }
    }
}

/// Minimal `Read` over a Win32 pipe HANDLE (the windows crate doesn't impl `Read` on HANDLE).
struct HandleReader(HANDLE);
unsafe impl Send for HandleReader {}
impl Read for HandleReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut read = 0u32;
        let ok = unsafe {
            windows::Win32::Storage::FileSystem::ReadFile(self.0, Some(buf), Some(&mut read), None)
        };
        match ok {
            Ok(()) => Ok(read as usize),
            // A broken pipe (helper exited) reads as ERROR_BROKEN_PIPE → report EOF (0).
            Err(_) => Ok(0),
        }
    }
}
impl Drop for HandleReader {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Is this process running as the LOCAL SYSTEM account? Used to decide whether the two-process
/// secure-desktop path applies (only SYSTEM can `WTSQueryUserToken` + capture the Winlogon desktop).
pub fn running_as_system() -> bool {
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        if len == 0 {
            let _ = CloseHandle(token);
            return false;
        }
        let mut buf = vec![0u8; len as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut _),
            len,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        if !ok {
            return false;
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        // The well-known LocalSystem SID is S-1-5-18.
        is_local_system_sid(tu.User.Sid)
    }
}

/// True iff `sid` is S-1-5-18 (LocalSystem).
unsafe fn is_local_system_sid(sid: windows::Win32::Security::PSID) -> bool {
    use windows::Win32::Security::{
        GetSidIdentifierAuthority, GetSidSubAuthority, GetSidSubAuthorityCount, IsValidSid,
    };
    if !IsValidSid(sid).as_bool() {
        return false;
    }
    let auth = GetSidIdentifierAuthority(sid);
    if auth.is_null() {
        return false;
    }
    // NT Authority = {0,0,0,0,0,5}.
    let a = (*auth).Value;
    if a != [0, 0, 0, 0, 0, 5] {
        return false;
    }
    let count = *GetSidSubAuthorityCount(sid);
    if count != 1 {
        return false;
    }
    *GetSidSubAuthority(sid, 0) == 18 // SECURITY_LOCAL_SYSTEM_RID
}
