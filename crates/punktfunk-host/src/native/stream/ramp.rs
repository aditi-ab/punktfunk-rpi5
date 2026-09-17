//! Serving the client's bring-up ramp on the idle data plane.
//!
//! The data `Session` is built the moment the client's punch lands and then
//! sits untouched for the two to three seconds `StreamState::new` spends on
//! the display, the pipeline and the launch. [`RampServer`] borrows it for
//! that gap on its own thread, answers the short `ProbeRequest`s the client's
//! ramp sends, and hands both the session and the request channel back
//! unchanged for the send thread ([`HOST_CAP2_RAMP`]).
//!
//! [`HOST_CAP2_RAMP`]: punktfunk_core::quic::HOST_CAP2_RAMP

use super::*;

/// Longest step the ramp window serves. The client's steps are 25 ms; this
/// bounds what a hand-over waits for, and what an exemption from the control
/// task's spacing can cost.
pub(crate) const RAMP_STEP_MAX_MS: u32 = 50;

/// The ramp window, serving until [`finish`](RampServer::finish) takes the
/// session back. Dropping it instead ends the window and lets both go.
pub(super) struct RampServer {
    done: Arc<AtomicBool>,
    open: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<(Session, ProbeReceiver)>>,
    /// What `start` was handed, when there was nothing to serve on.
    idle: Option<(Session, ProbeReceiver)>,
}

type ProbeReceiver = std::sync::mpsc::Receiver<ProbeRequest>;

impl RampServer {
    /// Take the session for the bring-up gap. `open` is the flag the control
    /// task reads to let ramp steps past its one-per-10 s spacing; it is
    /// cleared on hand-over, because from then on the send loop owns the
    /// session and video is about to leave.
    pub(super) fn start(
        session: Session,
        probe_rx: ProbeReceiver,
        probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
        probe_seq: bool,
        stop: Arc<AtomicBool>,
        open: Arc<AtomicBool>,
    ) -> Self {
        // A client whose reassembler cannot window probe frames has no ramp to
        // serve, and the spawn is pure cost.
        if !probe_seq || !open.load(Ordering::SeqCst) {
            open.store(false, Ordering::SeqCst);
            return RampServer {
                done: Arc::new(AtomicBool::new(true)),
                open,
                thread: None,
                idle: Some((session, probe_rx)),
            };
        }
        let done = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("punktfunk-ramp".into())
            .spawn({
                let done = done.clone();
                move || serve(session, probe_rx, &probe_result_tx, &done, &stop)
            })
            .map_err(|e| tracing::warn!(error = %e, "bring-up ramp thread not started"))
            .ok();
        if thread.is_none() {
            open.store(false, Ordering::SeqCst);
        }
        RampServer {
            done,
            open,
            thread,
            idle: None,
        }
    }

    /// The pipeline is ready: finish the step in flight, join, and give the
    /// session back for the send thread.
    pub(super) fn finish(mut self) -> (Session, ProbeReceiver) {
        self.open.store(false, Ordering::SeqCst);
        self.done.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            match t.join() {
                Ok(pair) => return pair,
                // The session went with it. Nothing below can stream, and the
                // caller's `?` is the honest end.
                Err(_) => panic!("the bring-up ramp thread panicked"),
            }
        }
        self.idle
            .take()
            .expect("a server with no thread holds both")
    }
}

impl Drop for RampServer {
    /// An early return out of bring-up: end the window and let the thread go
    /// with the session it holds.
    fn drop(&mut self) {
        self.open.store(false, Ordering::SeqCst);
        self.done.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Serve requests until the pipeline is ready. One burst at a time, each
/// clamped to [`RAMP_STEP_MAX_MS`], so the hand-over never waits long.
fn serve(
    mut session: Session,
    probe_rx: ProbeReceiver,
    probe_result_tx: &tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    done: &AtomicBool,
    stop: &AtomicBool,
) -> (Session, ProbeReceiver) {
    let mut served = 0u32;
    while !done.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
        let Ok(req) = probe_rx.recv_timeout(std::time::Duration::from_millis(2)) else {
            continue; // timeout, or the control task is gone — the loop's own flags end it
        };
        let req = ProbeRequest {
            duration_ms: req.duration_ms.min(RAMP_STEP_MAX_MS),
            ..req
        };
        let result = match ProbeBurst::begin(req, true) {
            Some(mut burst) => {
                while !burst.expired() && !stop.load(Ordering::SeqCst) {
                    burst.pump(&mut session);
                    std::thread::sleep(burst.next_due().min(std::time::Duration::from_micros(200)));
                }
                served += 1;
                burst.finish()
            }
            None => declined(),
        };
        let _ = probe_result_tx.send(result);
    }
    if served > 0 {
        tracing::info!(
            steps = served,
            "served the client's bring-up ramp while the pipeline built"
        );
    }
    (session, probe_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback_host() -> (punktfunk_core::transport::LoopbackTransport, Session) {
        use punktfunk_core::config::{Config, Role};
        let (host_tp, client_tp) = punktfunk_core::transport::loopback_pair(0, 0);
        (
            client_tp,
            Session::new(Config::p1_defaults(Role::Host), Box::new(host_tp)).expect("host session"),
        )
    }

    /// The ramp's steps are served on the idle plane and both halves come
    /// back for the send thread.
    #[test]
    fn steps_are_served_before_the_pipeline_exists() {
        let (_client, session) = loopback_host();
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        let (res_tx, mut res_rx) = tokio::sync::mpsc::unbounded_channel();
        let open = Arc::new(AtomicBool::new(true));
        let ramp = RampServer::start(
            session,
            req_rx,
            res_tx,
            true,
            Arc::new(AtomicBool::new(false)),
            open.clone(),
        );
        for target_kbps in [5_000, 10_000] {
            req_tx
                .send(ProbeRequest {
                    target_kbps,
                    duration_ms: 25,
                })
                .expect("the server is listening");
        }
        // Both results, then the hand-over: the step in flight finishes first.
        let mut results = Vec::new();
        while results.len() < 2 {
            if let Ok(r) = res_rx.try_recv() {
                results.push(r);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let (_session, _rx) = ramp.finish();
        assert!(
            !open.load(Ordering::SeqCst),
            "the window closes at the hand-over, so the spacing is back"
        );
        for r in &results {
            assert!(
                r.bytes_sent > 0,
                "a step that sent nothing measures nothing"
            );
            assert!(r.wire_packets_sent > 0);
            assert!(r.duration_ms <= 2 * RAMP_STEP_MAX_MS, "{r:?}");
        }
        assert!(
            results[1].bytes_sent > results[0].bytes_sent,
            "twice the rate over the same step is twice the bytes: {results:?}"
        );
    }

    /// The pipeline becomes ready mid-step: the step in flight finishes, and
    /// the hand-over waits no longer than one step for it.
    #[test]
    fn a_hand_over_mid_step_waits_for_that_step_and_no_more() {
        let (_client, session) = loopback_host();
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        let (res_tx, mut res_rx) = tokio::sync::mpsc::unbounded_channel();
        let ramp = RampServer::start(
            session,
            req_rx,
            res_tx,
            true,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
        );
        req_tx
            .send(ProbeRequest {
                target_kbps: 40_000,
                duration_ms: RAMP_STEP_MAX_MS,
            })
            .expect("the server is listening");
        std::thread::sleep(std::time::Duration::from_millis(5)); // mid-step
        let started = std::time::Instant::now();
        let (session, _rx) = ramp.finish();
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_millis(2 * u64::from(RAMP_STEP_MAX_MS)),
            "the hand-over waited {took:?}"
        );
        assert!(
            session.stats().packets_sent > 0,
            "the step in flight is finished, not abandoned"
        );
        assert!(res_rx.try_recv().is_ok(), "and it is reported");
    }

    /// Bring-up must not get slower: with nothing to serve, the window is a
    /// thread spawn and a join.
    #[test]
    fn an_unused_window_costs_bring_up_nothing() {
        let (_client, session) = loopback_host();
        let (_req_tx, req_rx) = std::sync::mpsc::channel();
        let (res_tx, _res_rx) = tokio::sync::mpsc::unbounded_channel();
        let started = std::time::Instant::now();
        let ramp = RampServer::start(
            session,
            req_rx,
            res_tx,
            true,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
        );
        let (session, _rx) = ramp.finish();
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_millis(50), "took {took:?}");
        assert_eq!(session.stats().packets_sent, 0);
    }

    /// An old client asks for nothing and its plane must be handed over
    /// untouched — no thread, no window, and the spacing left alone.
    #[test]
    fn a_client_that_cannot_window_probe_frames_gets_no_window() {
        let (_client, session) = loopback_host();
        let (_req_tx, req_rx) = std::sync::mpsc::channel();
        let (res_tx, mut res_rx) = tokio::sync::mpsc::unbounded_channel();
        let open = Arc::new(AtomicBool::new(true));
        let ramp = RampServer::start(
            session,
            req_rx,
            res_tx,
            false,
            Arc::new(AtomicBool::new(false)),
            open.clone(),
        );
        assert!(!open.load(Ordering::SeqCst));
        let (session, _rx) = ramp.finish();
        assert_eq!(session.stats().packets_sent, 0);
        assert!(res_rx.try_recv().is_err());
    }

    /// A step longer than the window serves is clamped, so a hand-over never
    /// waits on an 800 ms burst that arrived at the wrong moment.
    #[test]
    fn a_long_burst_is_cut_to_the_windows_step() {
        let (_client, session) = loopback_host();
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        let (res_tx, mut res_rx) = tokio::sync::mpsc::unbounded_channel();
        let ramp = RampServer::start(
            session,
            req_rx,
            res_tx,
            true,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
        );
        req_tx
            .send(ProbeRequest {
                target_kbps: 20_000,
                duration_ms: 800,
            })
            .expect("the server is listening");
        let started = std::time::Instant::now();
        let r = loop {
            if let Ok(r) = res_rx.try_recv() {
                break r;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_millis(400),
                "an 800 ms burst was not clamped"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        let _ = ramp.finish();
        assert!(r.duration_ms <= 2 * RAMP_STEP_MAX_MS, "{r:?}");
    }
}
