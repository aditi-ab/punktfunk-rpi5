//! Session-transition latency trace (design/first-frame-and-resize-latency.md P0.1).
//!
//! One [`Trace`] per transition — session bring-up (`hello → … → first_packet`) or a mid-stream
//! resize (`reconfigure_received → … → pipeline_rebuilt`) — collects millisecond stage stamps
//! across the threads a transition crosses (handshake task, display-prep/encode thread, send
//! thread) and emits ONE summary `info!` line when the transition completes, so every landed
//! latency change is measured against a number instead of vibes. The completed total also lands
//! in a shared slot [`crate::session_status`] exposes (`time_to_first_frame_ms` /
//! `last_resize_ms`), so the web-console Dashboard and future regressions can read it per session.
//!
//! Deliberately coarse: stages are stamped where the session layer can see them; layers the trace
//! doesn't reach (the Windows display manager's activation ladder / settle waits) log their own
//! per-stage deltas and correlate by wall clock.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A single transition's stage trace. Cheap and thread-safe: `mark` is a mutex push, `finish`
/// emits the one summary line (exactly once — later calls no-op, so an abandoned trace stays
/// silent).
pub(crate) struct Trace {
    /// Which transition this traces (`"bringup"` / `"resize"`) — the summary line's `kind`.
    kind: &'static str,
    origin: Instant,
    /// `(stage, ms since origin)` in stamp order.
    stages: Mutex<Vec<(&'static str, u32)>>,
    finished: AtomicBool,
    /// Where the completed total lands — shared with [`crate::session_status`].
    total_ms: Arc<AtomicU32>,
}

impl Trace {
    /// Start a trace at "now" (= the first stage's zero point). `total_ms` is the shared slot the
    /// completed total is stored into (0 until the transition finishes).
    pub(crate) fn start(kind: &'static str, total_ms: Arc<AtomicU32>) -> Arc<Self> {
        Arc::new(Self {
            kind,
            origin: Instant::now(),
            stages: Mutex::new(Vec::new()),
            finished: AtomicBool::new(false),
            total_ms,
        })
    }

    /// The shared slot the completed total is stored into (for `session_status::register`).
    pub(crate) fn total_slot(&self) -> Arc<AtomicU32> {
        self.total_ms.clone()
    }

    /// Stamp a stage at "now" — first occurrence only (a retried build re-crosses its stamp
    /// points; the first crossing is the one the transition timeline wants). No-op after
    /// [`finish`](Self::finish), so steady-state paths that also cross a stamped point stay free.
    pub(crate) fn mark(&self, stage: &'static str) {
        if self.finished.load(Ordering::Relaxed) {
            return;
        }
        let ms = self.origin.elapsed().as_millis().min(u32::MAX as u128) as u32;
        let mut stages = self.stages.lock().unwrap();
        if stages.iter().any(|(s, _)| *s == stage) {
            return;
        }
        stages.push((stage, ms));
    }

    /// Stamp the final stage and emit the one-line summary (first call only). The final stage's
    /// offset is the transition total, stored into the shared slot.
    pub(crate) fn finish(&self, stage: &'static str) {
        if self.finished.swap(true, Ordering::Relaxed) {
            return;
        }
        let total = self.origin.elapsed().as_millis().min(u32::MAX as u128) as u32;
        let mut stages = self.stages.lock().unwrap();
        stages.push((stage, total));
        let line = stages
            .iter()
            .map(|(s, ms)| format!("{s}+{ms}"))
            .collect::<Vec<_>>()
            .join(" ");
        drop(stages);
        self.total_ms.store(total.max(1), Ordering::Relaxed);
        tracing::info!(
            kind = self.kind,
            total_ms = total,
            stages = %line,
            "session-transition trace"
        );
    }
}
