//! Phase A.3 DxgKrnl ETW correlation (stall attribution, `vdisplay-disturbance-immunity.md`
//! §4.3): a tiny real-time ETW session on `Microsoft-Windows-DxgKrnl`, event-id-filtered to the
//! five display-miniport DDI families whose servicing freezes the present path, so a stall report
//! can NAME the DDI (and its duration bracket) instead of saying "below Windows":
//!
//! - 150/151 `QueryChildStatus` start/stop — connector polling (the DDC/child-I/O class),
//! - 272 `IndicateChildStatus` — the miniport itself reporting a connector change,
//! - 154/155 `SetPowerState` start/stop — monitor/link power transitions (Class-1 servicing),
//! - 430 `SetTimingsFromVidPn` — modeset-class commits (Level-Two "hardware is idle" freezes),
//! - 1096/1097 `DisplayDetectControl` start/stop — present on newer builds; filtered in
//!   unconditionally, harmless where absent,
//! - 1071 `BltQueueAddEntry` — one event per frame ENTERING the virtual display's kernel present
//!   queue (the modern IDD path: DWM present → PresentRedirected → BltQueue → soft-vsync →
//!   `BltQueueCompleteIndirectPresent` → IddCx → our driver; anatomy proven on-glass via xperf,
//!   2026-07-30 — its gaps matched our capture holes to the millisecond),
//! - 1068 `BltQueueCompleteIndirectPresent` — one event per frame COMPLETED to IddCx.
//!
//! A second provider rides the same session: `Microsoft-Windows-DXGI` (user-mode), filtered to
//! `Present`/`PresentMultiplaneOverlay` starts (ids 42/55) — one event per swapchain present,
//! stamped with the PRESENTING process id. Together they are the compose-silence discriminator
//! ([`EtwWatch::window_counts`]): DXGI presents flowing while `BltQueueAddEntry` gaps = the OS
//! display path dropped composed frames (the real display-path bug); BOTH silent = the content
//! stopped presenting (benign pause — menus/loading/game hitch). The predecessor witnesses are
//! retired for cause: DxgKrnl id 184 `Present` never fires on the modern redirected path, and
//! `DWM_TIMING_INFO.cFrame` is refresh-synthesized on Win11 (advances without composes) — both
//! proven on-glass 2026-07-30.
//!
//! Kernel-side event-id filtering (`EVENT_FILTER_TYPE_EVENT_ID`) keeps the per-vblank firehose
//! off; the DDI families cost a few events per minute, and the present/queue streams a few
//! hundred per second (bounded by the ring + the 64 KB session buffers). Starting a real-time
//! session needs admin / Performance Log Users — the packaged host (service, SYSTEM) has it; a
//! plain dev run degrades to `None` and every report says `etw=unavailable` instead of guessing.
//! The session's `FlushTimer` is 1 s, so a bracket from the trailing second of a gap can land
//! AFTER that stall's report line — the next report (and the metronomic tally) still carries it.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use windows::core::{GUID, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS};
use windows::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW,
    CONTROLTRACE_HANDLE, ENABLE_TRACE_PARAMETERS, ENABLE_TRACE_PARAMETERS_VERSION_2,
    EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_FILTER_DESCRIPTOR, EVENT_FILTER_TYPE_EVENT_ID,
    EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, PROCESSTRACE_HANDLE, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_REAL_TIME, TRACE_LEVEL_INFORMATION, WNODE_FLAG_TRACED_GUID,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// `Microsoft-Windows-DxgKrnl` (`{802EC45A-1E99-4B83-9920-87C98277BA9D}`).
const DXGKRNL: GUID = GUID::from_u128(0x802EC45A_1E99_4B83_9920_87C98277BA9D);

/// `Microsoft-Windows-DXGI` (`{CA11C036-0102-4A2D-A6AD-F03CFED5D3C9}`) — the user-mode swapchain
/// runtime; its `Present` events fire in the presenting process's context.
const DXGI: GUID = GUID::from_u128(0xCA11C036_0102_4A2D_A6AD_F03CFED5D3C9);

/// The DxgKrnl event ids the session filters IN (see the module docs).
const FILTER_IDS: [u16; 10] = [
    150,
    151,
    272,
    154,
    155,
    430,
    1096,
    1097,
    BLT_ADD_ID,
    BLT_COMPLETE_ID,
];

/// `BltQueueAddEntry` — a frame ENTERED the virtual display's kernel present queue. Event id
/// resolved from the 26200 manifest (task 1105) and count-verified against an on-glass capture.
const BLT_ADD_ID: u16 = 1071;

/// `BltQueueCompleteIndirectPresent` — a frame COMPLETED from the queue to IddCx (task 1059).
const BLT_COMPLETE_ID: u16 = 1068;

/// The DXGI event ids the session filters IN: `Present` start (42) and
/// `PresentMultiplaneOverlay` start (55) — one per app/DWM present call.
const DXGI_FILTER_IDS: [u16; 2] = [DXGI_PRESENT_ID, DXGI_PRESENT_MPO_ID];
const DXGI_PRESENT_ID: u16 = 42;
const DXGI_PRESENT_MPO_ID: u16 = 55;

/// Session name — ours, stopped-if-stale at start (a crashed host leaves the session behind;
/// real-time sessions are machine-global named objects).
const SESSION: &str = "punktfunk-stallwatch-dxgkrnl";

/// The consumer callback's destination: `(event QPC, event id, process id)`, capped. The DDI
/// families are a few events per minute; the DXGI present stream and the two BltQueue streams
/// each run at compose rate, so the cap sizes the history: 16384 entries ≈ 15–60 s at a
/// 90–240 Hz desktop under a game + DWM — comfortably more than any single stall window the
/// reporter asks about (a deeper hole's counts read as floors, which is still evidence).
/// Event-id spaces of the two providers are disjoint (42/55 vs 15x/2xx/4xx/10xx/1071/1068), so
/// one ring holds both without a provider tag.
static RING: Mutex<VecDeque<(i64, u16, u32)>> = Mutex::new(VecDeque::new());

/// [`RING`]'s cap (see its doc for the sizing math).
const RING_CAP: usize = 16384;

fn qpc_now() -> i64 {
    let mut v = 0i64;
    // SAFETY: plain FFI; `v` is a valid local out-param.
    let _ = unsafe { QueryPerformanceCounter(&mut v) };
    v
}

fn qpc_freq() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: plain FFI; `f` is a valid local out-param; fixed at boot.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    })
}

/// The consumer's per-event callback — record id + QPC timestamp (the session's `ClientContext`
/// is 1, so `TimeStamp` IS a QPC value) and return; runs on the consumer thread.
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    // SAFETY: `record` is the live event the consumer is delivering for the duration of this call.
    let (id, ts, pid) = unsafe {
        (
            (*record).EventHeader.EventDescriptor.Id,
            (*record).EventHeader.TimeStamp,
            (*record).EventHeader.ProcessId,
        )
    };
    let mut ring = RING.lock().unwrap();
    if ring.len() == RING_CAP {
        ring.pop_front();
    }
    ring.push_back((ts, id, pid));
}

/// A live DxgKrnl watch: the controller handle (stops the session on drop) + the consumer handle.
pub(super) struct EtwWatch {
    session: CONTROLTRACE_HANDLE,
    consumer: PROCESSTRACE_HANDLE,
}

// SAFETY: both fields are plain kernel handle VALUES (u64 wrappers) owned by this watch; every
// operation on them (summary reads the static ring; Drop stops/closes) is thread-safe by the ETW
// API contract, and the singleton hands out only `Arc<EtwWatch>`.
unsafe impl Send for EtwWatch {}
// SAFETY: as above — `&EtwWatch` exposes only `summary` (static-ring reads).
unsafe impl Sync for EtwWatch {}

static WATCH: Mutex<Weak<EtwWatch>> = Mutex::new(Weak::new());

/// The process-wide watch, started on first use; `None` when the session cannot start (no admin
/// rights on a dev run) — callers report `etw=unavailable` rather than guessing.
pub(super) fn acquire() -> Option<Arc<EtwWatch>> {
    let mut g = WATCH.lock().unwrap();
    if let Some(w) = g.upgrade() {
        return Some(w);
    }
    let w = Arc::new(EtwWatch::start()?);
    *g = Arc::downgrade(&w);
    Some(w)
}

/// An `EVENT_TRACE_PROPERTIES` allocation with the session-name space ETW writes into appended —
/// the canonical controller-buffer pattern.
fn properties_buffer() -> (Vec<u8>, usize) {
    let base = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
    let total = base + (SESSION.len() + 1) * 2;
    (vec![0u8; total], base)
}

impl EtwWatch {
    fn start() -> Option<Self> {
        let name: Vec<u16> = SESSION.encode_utf16().chain([0]).collect();
        // A stale session from a crashed host blocks StartTrace with ERROR_ALREADY_EXISTS — stop
        // it by name first (fails benignly when there is none).
        let (mut stop_buf, _) = properties_buffer();
        // SAFETY: `stop_buf` is a live, zeroed, correctly-sized properties allocation; the name is
        // a live nul-terminated wide string; a session handle of 0 + name = control-by-name.
        unsafe {
            let props = stop_buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
            (*props).Wnode.BufferSize = stop_buf.len() as u32;
            let _ = ControlTraceW(
                CONTROLTRACE_HANDLE::default(),
                PWSTR(name.as_ptr() as *mut _),
                props,
                EVENT_TRACE_CONTROL_STOP,
            );
        }

        let (mut buf, base) = properties_buffer();
        let mut session = CONTROLTRACE_HANDLE::default();
        // SAFETY: `buf` is a live, zeroed allocation of base + name bytes; every write below is a
        // field of the properties struct at its head; `LoggerNameOffset = base` points at the
        // appended name space (ETW copies the name there itself). ClientContext 1 = QPC clock —
        // what makes event timestamps comparable to our probe windows.
        let rc = unsafe {
            let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
            (*props).Wnode.BufferSize = buf.len() as u32;
            (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*props).Wnode.ClientContext = 1;
            (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
            (*props).BufferSize = 64;
            (*props).MinimumBuffers = 2;
            (*props).MaximumBuffers = 4;
            (*props).FlushTimer = 1;
            (*props).LoggerNameOffset = base as u32;
            StartTraceW(&mut session, PWSTR(name.as_ptr() as *mut _), props)
        };
        if rc != ERROR_SUCCESS {
            tracing::debug!(
                rc = rc.0,
                "DxgKrnl ETW session unavailable (needs admin / Performance Log Users) — \
                 stall reports will say etw=unavailable"
            );
            return None;
        }

        // Enable DxgKrnl with a kernel-side event-id filter — the whole point: the provider's
        // vblank/DPC keywords never reach us. Fatal on failure (the DDI families + queue
        // witnesses are this watch's reason to exist).
        if !enable_provider(session, &DXGKRNL, &FILTER_IDS) {
            tracing::debug!("DxgKrnl ETW enable failed — stopping the session");
            let (mut buf, _) = properties_buffer();
            // SAFETY: live handle + valid properties allocation, stopped exactly once on this path.
            unsafe {
                let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
                (*props).Wnode.BufferSize = buf.len() as u32;
                let _ = ControlTraceW(session, PWSTR::null(), props, EVENT_TRACE_CONTROL_STOP);
            }
            return None;
        }
        // The DXGI (user-mode) present witness rides the same session. Degraded-not-fatal: a
        // refusal only costs the per-process present counts — `window_counts` then reports
        // no present history and classification stays honest (Unattributed, never a guess).
        if !enable_provider(session, &DXGI, &DXGI_FILTER_IDS) {
            tracing::debug!(
                "DXGI ETW enable failed — present-vs-queue discrimination unavailable this session"
            );
        }

        let mut log = EVENT_TRACE_LOGFILEW {
            LoggerName: PWSTR(name.as_ptr() as *mut _),
            ..Default::default()
        };
        log.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        log.Anonymous2.EventRecordCallback = Some(on_event);
        // SAFETY: `log` is a fully-initialized local; `name` outlives the call (OpenTrace copies
        // what it needs before returning).
        let consumer = unsafe { OpenTraceW(&mut log) };
        if consumer.Value == u64::MAX {
            tracing::debug!("DxgKrnl ETW OpenTrace failed — stopping the session");
            let (mut buf, _) = properties_buffer();
            // SAFETY: live handle + valid properties allocation, stopped exactly once on this path.
            unsafe {
                let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
                (*props).Wnode.BufferSize = buf.len() as u32;
                let _ = ControlTraceW(session, PWSTR::null(), props, EVENT_TRACE_CONTROL_STOP);
            }
            return None;
        }
        // The consumer: ProcessTrace blocks for the session's lifetime; Drop's STOP unblocks it.
        let consumer_value = consumer.Value;
        if let Err(e) = std::thread::Builder::new()
            .name("pf-etw-dxgkrnl".into())
            .spawn(move || {
                // SAFETY: `consumer_value` is the live consumer handle opened above; ProcessTrace
                // pumps it until the controller stops the session (Drop), then returns.
                let rc = unsafe {
                    ProcessTrace(
                        &[PROCESSTRACE_HANDLE {
                            Value: consumer_value,
                        }],
                        None,
                        None,
                    )
                };
                tracing::debug!(rc = rc.0, "DxgKrnl ETW consumer exited");
            })
        {
            tracing::debug!(error = %e, "DxgKrnl ETW consumer thread failed to spawn");
            let (mut buf, _) = properties_buffer();
            // SAFETY: live handles + valid properties allocation, released exactly once on this path.
            unsafe {
                let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
                (*props).Wnode.BufferSize = buf.len() as u32;
                let _ = ControlTraceW(session, PWSTR::null(), props, EVENT_TRACE_CONTROL_STOP);
                let _ = CloseTrace(consumer);
            }
            return None;
        }
        tracing::debug!("DxgKrnl ETW stall-watch session live (event-id filtered)");
        Some(Self { session, consumer })
    }

    /// Summarize the DDI activity inside `[from, to]` — the correlation line a stall report
    /// carries. Brackets that merely SPAN the window count too (a freeze-long `SetPowerState`
    /// has both edges outside the hole it caused). `"none"` when the window is clean.
    pub(super) fn summary(&self, from: Instant, to: Instant) -> String {
        // Instant → QPC: anchor both clocks now and offset backwards.
        let (now_i, now_q, freq) = (Instant::now(), qpc_now(), qpc_freq());
        let to_q = now_q - duration_qpc(now_i.saturating_duration_since(to), freq);
        let from_q = now_q - duration_qpc(now_i.saturating_duration_since(from), freq);
        let events: Vec<(i64, u16, u32)> = {
            let ring = RING.lock().unwrap();
            ring.iter()
                .filter(|(ts, _, _)| *ts <= to_q)
                .copied()
                .collect()
        };
        let ms = |dq: i64| dq.max(0) * 1_000 / freq;
        let mut parts = Vec::new();
        for (start_id, stop_id, label) in [
            (150u16, 151u16, "QueryChildStatus"),
            (154, 155, "SetPowerState"),
            (1096, 1097, "DisplayDetectControl"),
        ] {
            let mut open: Option<i64> = None;
            let mut count = 0u32;
            let mut max_ms = 0i64;
            let mut still_open = false;
            for &(ts, id, _) in &events {
                if id == start_id {
                    open = Some(ts);
                } else if id == stop_id {
                    if let Some(s) = open.take() {
                        // The bracket [s, ts] counts when it intersects the window.
                        if s <= to_q && ts >= from_q {
                            count += 1;
                            max_ms = max_ms.max(ms(ts - s));
                        }
                    }
                }
            }
            if let Some(s) = open {
                if s <= to_q {
                    count += 1;
                    max_ms = max_ms.max(ms(to_q - s));
                    still_open = true;
                }
            }
            if count > 0 {
                parts.push(format!(
                    "{label}×{count}(max {max_ms}ms{})",
                    if still_open { ", open" } else { "" }
                ));
            }
        }
        for (id, label) in [
            (272u16, "IndicateChildStatus"),
            (430, "SetTimingsFromVidPn"),
        ] {
            let count = events
                .iter()
                .filter(|(ts, i, _)| *i == id && *ts >= from_q && *ts <= to_q)
                .count();
            if count > 0 {
                parts.push(format!("{label}×{count}"));
            }
        }
        // Present + queue accounting (DXGI 42/55 + BltQueueAddEntry/Complete): total presents
        // inside the window plus the top presenters, NAMED — the line that splits a
        // compose-silence hole into "the content stopped presenting" (no presents anywhere)
        // versus "presents flowed and the display path dropped them" (presents at rate while
        // the queue starves). "Present×0" is printed explicitly when the stream has history
        // but the window is empty — silence is a finding, not an absence.
        let mut per_pid: Vec<(u32, u32)> = Vec::new();
        let mut have_present_history = false;
        let (mut adds, mut completes) = (0u32, 0u32);
        let mut have_queue_history = false;
        for &(ts, id, pid) in &events {
            match id {
                DXGI_PRESENT_ID | DXGI_PRESENT_MPO_ID => {
                    have_present_history = true;
                    if ts >= from_q && ts <= to_q {
                        match per_pid.iter_mut().find(|(p, _)| *p == pid) {
                            Some((_, c)) => *c += 1,
                            None => per_pid.push((pid, 1)),
                        }
                    }
                }
                BLT_ADD_ID | BLT_COMPLETE_ID => {
                    have_queue_history = true;
                    if ts >= from_q && ts <= to_q {
                        if id == BLT_ADD_ID {
                            adds += 1;
                        } else {
                            completes += 1;
                        }
                    }
                }
                _ => {}
            }
        }
        if !per_pid.is_empty() {
            per_pid.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
            let total: u32 = per_pid.iter().map(|(_, c)| c).sum();
            let top = per_pid
                .iter()
                .take(2)
                .map(|(pid, c)| {
                    let name = process_name(*pid).unwrap_or_else(|| format!("pid{pid}"));
                    format!("{name}:{c}")
                })
                .collect::<Vec<_>>()
                .join(",");
            parts.push(format!("Present×{total}({top})"));
        } else if have_present_history {
            parts.push("Present×0".to_string());
        }
        if have_queue_history {
            parts.push(format!("blt-queue add×{adds} complete×{completes}"));
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(" ")
        }
    }

    /// The structured discriminator read for `[from, to]` (the stall classifier's evidence):
    /// how many swapchain presents (DXGI 42/55, any process) and how many virtual-display
    /// queue entries (`BltQueueAddEntry`) landed in the window, plus whether each stream has
    /// EVER produced an event (distinguishing a true zero from a witness that is not working —
    /// e.g. the DXGI enable was refused, or an OS build renumbered the BltQueue events).
    pub(super) fn window_counts(&self, from: Instant, to: Instant) -> EtwWindowCounts {
        let (now_i, now_q, freq) = (Instant::now(), qpc_now(), qpc_freq());
        let to_q = now_q - duration_qpc(now_i.saturating_duration_since(to), freq);
        let from_q = now_q - duration_qpc(now_i.saturating_duration_since(from), freq);
        let ring = RING.lock().unwrap();
        let mut out = EtwWindowCounts::default();
        for &(ts, id, _) in ring.iter() {
            match id {
                DXGI_PRESENT_ID | DXGI_PRESENT_MPO_ID => {
                    out.present_history = true;
                    if ts >= from_q && ts <= to_q {
                        out.presents += 1;
                    }
                }
                BLT_ADD_ID => {
                    out.queue_history = true;
                    if ts >= from_q && ts <= to_q {
                        out.queue_adds += 1;
                    }
                }
                _ => {}
            }
        }
        out
    }
}

/// [`EtwWatch::window_counts`]'s read: the compose-silence discriminator's structured evidence.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct EtwWindowCounts {
    /// Swapchain presents (any process — the game AND dwm both count) inside the window.
    pub(super) presents: u32,
    /// `BltQueueAddEntry` events (frames entering the virtual display's kernel queue) inside it.
    pub(super) queue_adds: u32,
    /// The present stream has produced at least one event EVER (witness known-working).
    pub(super) present_history: bool,
    /// The queue stream has produced at least one event EVER (witness known-working).
    pub(super) queue_history: bool,
}

/// Enable `guid` on `session` with a kernel-side event-id allowlist. `true` on success.
fn enable_provider(session: CONTROLTRACE_HANDLE, guid: &GUID, ids: &[u16]) -> bool {
    let mut filter = Vec::with_capacity(4 + ids.len() * 2);
    filter.extend_from_slice(&[1u8, 0u8]); // FilterIn = TRUE, Reserved
    filter.extend_from_slice(&(ids.len() as u16).to_le_bytes());
    for id in ids {
        filter.extend_from_slice(&id.to_le_bytes());
    }
    let mut desc = EVENT_FILTER_DESCRIPTOR {
        Ptr: filter.as_ptr() as u64,
        Size: filter.len() as u32,
        Type: EVENT_FILTER_TYPE_EVENT_ID,
    };
    let params = ENABLE_TRACE_PARAMETERS {
        Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
        EnableProperty: 0,
        ControlFlags: 0,
        SourceId: GUID::zeroed(),
        EnableFilterDesc: &mut desc,
        FilterDescCount: 1,
    };
    // SAFETY: `session` is a live handle owned by the caller; `params`/`desc`/`filter` are live
    // locals for this synchronous call (the kernel copies the filter before returning).
    let rc = unsafe {
        EnableTraceEx2(
            session,
            guid,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
            TRACE_LEVEL_INFORMATION as u8,
            0,
            0,
            0,
            Some(&params),
        )
    };
    rc == ERROR_SUCCESS
}

/// Best-effort image base name for `pid` (Present attribution); `None` when the process is gone
/// or protected. Runs only at stall-report time — a rare, already-degraded moment.
fn process_name(pid: u32) -> Option<String> {
    // SAFETY: plain FFI; a refused open returns Err (checked via `ok()?`), and the returned
    // handle is closed exactly once below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: `process` is the live handle just opened with QUERY_LIMITED (what this API needs);
    // `buf`/`len` are a valid out-buffer and its capacity, `len` updated to the written UTF-16
    // length (no NUL) on success.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_ok();
    // SAFETY: `process` is the handle opened above, closed exactly once here.
    unsafe {
        let _ = CloseHandle(process);
    }
    if !ok {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    Some(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string())
}

/// A `Duration` in QPC ticks (saturating; diagnostic precision).
fn duration_qpc(d: Duration, freq: i64) -> i64 {
    (d.as_micros() as i64).saturating_mul(freq) / 1_000_000
}

impl Drop for EtwWatch {
    fn drop(&mut self) {
        let (mut buf, _) = properties_buffer();
        // SAFETY: `self.session`/`self.consumer` are the live handles this watch owns; the STOP
        // (with a valid properties allocation) ends the session and unblocks ProcessTrace, and
        // CloseTrace releases the consumer — each exactly once, here.
        unsafe {
            let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
            (*props).Wnode.BufferSize = buf.len() as u32;
            let _ = ControlTraceW(self.session, PWSTR::null(), props, EVENT_TRACE_CONTROL_STOP);
            let _ = CloseTrace(self.consumer);
        }
    }
}
