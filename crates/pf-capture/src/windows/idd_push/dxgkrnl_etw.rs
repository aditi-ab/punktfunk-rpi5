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
//!   unconditionally, harmless where absent.
//!
//! Kernel-side event-id filtering (`EVENT_FILTER_TYPE_EVENT_ID`) keeps the per-vblank firehose
//! off; the session costs a few events per minute. Starting a real-time session needs admin /
//! Performance Log Users — the packaged host (service, SYSTEM) has it; a plain dev run degrades
//! to `None` and every report says `etw=unavailable` instead of guessing. The session's
//! `FlushTimer` is 1 s, so a bracket from the trailing second of a gap can land AFTER that
//! stall's report line — the next report (and the metronomic tally) still carries it.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use windows::core::{GUID, PWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW,
    CONTROLTRACE_HANDLE, ENABLE_TRACE_PARAMETERS, ENABLE_TRACE_PARAMETERS_VERSION_2,
    EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_FILTER_DESCRIPTOR, EVENT_FILTER_TYPE_EVENT_ID,
    EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, PROCESSTRACE_HANDLE, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_REAL_TIME, TRACE_LEVEL_INFORMATION, WNODE_FLAG_TRACED_GUID,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// `Microsoft-Windows-DxgKrnl` (`{802EC45A-1E99-4B83-9920-87C98277BA9D}`).
const DXGKRNL: GUID = GUID::from_u128(0x802EC45A_1E99_4B83_9920_87C98277BA9D);

/// The event ids the session filters IN (see the module docs).
const FILTER_IDS: [u16; 8] = [150, 151, 272, 154, 155, 430, 1096, 1097];

/// Session name — ours, stopped-if-stale at start (a crashed host leaves the session behind;
/// real-time sessions are machine-global named objects).
const SESSION: &str = "punktfunk-stallwatch-dxgkrnl";

/// The consumer callback's destination: `(event QPC, event id)`, capped. A few events per minute
/// in the field; the cap only matters under a detection storm — exactly when the tail is the
/// least interesting part.
static RING: Mutex<VecDeque<(i64, u16)>> = Mutex::new(VecDeque::new());

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
    let (id, ts) = unsafe {
        (
            (*record).EventHeader.EventDescriptor.Id,
            (*record).EventHeader.TimeStamp,
        )
    };
    let mut ring = RING.lock().unwrap();
    if ring.len() == 2048 {
        ring.pop_front();
    }
    ring.push_back((ts, id));
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
        // vblank/DPC keywords never reach us.
        let mut filter = Vec::with_capacity(4 + FILTER_IDS.len() * 2);
        filter.extend_from_slice(&[1u8, 0u8]); // FilterIn = TRUE, Reserved
        filter.extend_from_slice(&(FILTER_IDS.len() as u16).to_le_bytes());
        for id in FILTER_IDS {
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
        // SAFETY: `session` is the live handle just started; `params`/`desc`/`filter` are live
        // locals for this synchronous call (the kernel copies the filter).
        let rc = unsafe {
            EnableTraceEx2(
                session,
                &DXGKRNL,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                TRACE_LEVEL_INFORMATION as u8,
                0,
                0,
                0,
                Some(&params),
            )
        };
        if rc != ERROR_SUCCESS {
            tracing::debug!(
                rc = rc.0,
                "DxgKrnl ETW enable failed — stopping the session"
            );
            let (mut buf, _) = properties_buffer();
            // SAFETY: live handle + valid properties allocation, stopped exactly once on this path.
            unsafe {
                let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
                (*props).Wnode.BufferSize = buf.len() as u32;
                let _ = ControlTraceW(session, PWSTR::null(), props, EVENT_TRACE_CONTROL_STOP);
            }
            return None;
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
        let events: Vec<(i64, u16)> = {
            let ring = RING.lock().unwrap();
            ring.iter().filter(|(ts, _)| *ts <= to_q).copied().collect()
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
            for &(ts, id) in &events {
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
                .filter(|(ts, i)| *i == id && *ts >= from_q && *ts <= to_q)
                .count();
            if count > 0 {
                parts.push(format!("{label}×{count}"));
            }
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(" ")
        }
    }
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
