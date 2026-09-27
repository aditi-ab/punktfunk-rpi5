//! One stream's life: start, the session events, mode switches, and the quick-action ring.

use super::*;

impl StreamState {
    /// `wake` pushes a [`FrameWake`] as each decoded frame lands, via a forwarder that
    /// owns the pump's frame channel. The run loop can block in `wait_event_timeout`
    /// and still present the instant a frame arrives. The forwarder exits when the
    /// pump drops its sender.
    pub(super) fn new(
        params: SessionParams,
        force_software: Arc<AtomicBool>,
        wake: sdl3::event::EventSender,
        priority: PresentPriority,
        native_refresh_hz: u32,
    ) -> StreamState {
        let preset = params.preset.clone();
        // Rate we asked for, until Welcome resolves it. No frames flow before that,
        // so this only has to be sane, not right.
        let source_interval_ns = frame_interval_ns(params.mode.refresh_hz, native_refresh_hz);
        // Presenter's half of phase-locked capture: keep the Arc before the params move.
        // `None` when the session did not advertise the cap — the 1 Hz fold then skips it.
        let latch_grid = params.phase_lock.then(|| params.latch_grid.clone());
        let retry_params = params.clone();
        let handle = session::start(params);
        let (wake_tx, wake_rx) = async_channel::bounded(2);
        let pump_rx = handle.frames.clone();
        let _ = std::thread::Builder::new()
            .name("pf-frame-wake".into())
            .spawn(move || {
                pf_client_core::audio_rt::boost_and_log("frame-wake");
                while let Ok(f) = pump_rx.recv_blocking() {
                    let _ = wake_tx.force_send(f); // newest wins, like the pump's queue
                    let _ = wake.push_custom_event(FrameWake);
                }
            });
        StreamState {
            handle,
            frames: wake_rx,
            connector: None,
            capture: None,
            cursor_chan: None,
            access: pf_client_core::access::SessionAccess::default(),
            session_notice: None,
            touch_mouse: crate::touch::SteamTouchMouse::new(in_gamescope()),
            last_hint: None,
            hint_since: std::time::Instant::now(),
            last_user_motion: std::time::Instant::now(),
            warp_echo_until: std::time::Instant::now(),
            hint_override: false,
            sent_client_draws: None,
            force_software,
            canceled: false,
            ready_announced: false,
            mode_line: String::new(),
            fp_hex: String::new(),
            native_mode: (0, 0, 0),
            preset,
            latch_grid,
            clock_offset: None,
            video_e2e: None,
            hdr: false,
            hdr_untonemapped: false,
            win_import_us: Vec::with_capacity(256),
            win_submit_us: Vec::with_capacity(256),
            win_fence_us: Vec::with_capacity(256),
            win_acquire_us: Vec::with_capacity(256),
            win_present_us: Vec::with_capacity(256),
            win_start: Instant::now(),
            last_snap: None,
            facts: DecodeFacts::default(),
            health_seen: None,
            last_forced: 0,
            store: FrameStore::new(usize::from(priority.fifo_capacity())),
            clock: LatchClock::new(native_refresh_hz),
            pacer: SourcePacer::new(),
            source_interval_ns,
            gate: PresentGate::default(),
            cadence: CadenceProbe::new(),
            mode_period_ns: 1_000_000_000 / u64::from(native_refresh_hz.max(1)),
            margin_ns: 0,
            win_misses: 0,
            win_out_max: 0,
            win_steps: [0; 6],
            win_busy: [0; 2],
            busy_on: crate::vk::BusyOn::Fence,
            last_displayed_ns: 0,
            last_slot_ns: 0,
            busy_retry: false,
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            pyro_latency_forced: false,
            dmabuf_demoted: false,
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            pyro_present_warned: false,
            cpu_present_warned: false,
            hw_fails: 0,
            osd: Vec::new(),
            resize_pending: None,
            resize_sent_at: None,
            resize_requested: None,
            shown_mode: None,
            resize_overlay: ResizeIndicator::default(),
            last_video: None,
            params: retry_params,
        }
    }

    /// Stop the pump and join its thread before any device-wide idle: the pump submits
    /// decode work to the shared device. It notices `stop` within its 20 ms receive
    /// timeout; on a normal end it is already returning.
    pub(super) fn shutdown(mut self) {
        self.handle.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.handle.thread.take() {
            let _ = t.join();
        }
    }

    /// User exit: release capture, close with QUIT_CLOSE_CODE so the host tears down
    /// instead of lingering, stop the pump. The pump then emits `Ended(None)`.
    pub(super) fn request_quit(&mut self) {
        if let Some(cap) = &mut self.capture {
            cap.release(true);
        }
        if let Some(c) = &self.connector {
            c.disconnect_quit();
        }
        self.handle.stop.store(true, Ordering::SeqCst);
    }
}

/// Replace the params' requested w/h with the window's physical pixel size —
/// even-floored (the host's `validate_dimensions` rejects odd) and clamped to a
/// sane minimum — keeping the resolved refresh. Fullscreen, the window is the display.
pub(super) fn apply_match_window(
    params: &mut SessionParams,
    window: &sdl3::video::Window,
    render_scale: f64,
    max_dim: u32,
) {
    let (pw, ph) = window.size_in_pixels();
    // × the render scale (even + codec-clamped) so match-window supersamples like the
    // fixed-mode path; 1.0 leaves the window's native pixels.
    let (w, h) = punktfunk_core::render_scale::apply(pw, ph, render_scale, max_dim);
    params.mode.width = w;
    params.mode.height = h;
    tracing::info!(
        w,
        h,
        "match-window: requesting the scaled window pixel size"
    );
}

/// Follow the live mode slot (any accepted ack — follower, another trigger, or rollback).
pub(super) fn hud_mode_tick(
    st: &mut StreamState,
    window: &mut sdl3::video::Window,
    title_base: &str,
) {
    let Some(c) = &st.connector else {
        return;
    };
    let m = c.mode();
    if st.shown_mode.is_some_and(|prev| prev != m) {
        st.mode_line = format!("{}×{}@{}", m.width, m.height, m.refresh_hz);
        tracing::info!(mode = %st.mode_line, "stream mode switched");
        let _ = window.set_title(&format!("{title_base} · {}", st.mode_line));
        // A switch is a full host-side rebuild: the interval the cushion is bounded by
        // can change, and the gap must re-anchor the cadence estimate rather than slew over.
        st.source_interval_ns = frame_interval_ns(m.refresh_hz, 0);
        st.pacer.reset();
    }
    st.shown_mode = Some(m);
}

/// Fire the debounced `Reconfigure` once ~400 ms pass with no further resize events.
/// Physical pixels, even-floored, clamped ≥ 320×200; ≥ 1 s between requests (the accept
/// ack round-trips in milliseconds, so the spacing also keeps ~one request outstanding);
/// each distinct size requested at most once (rejected sizes and host-side rollbacks).
pub(super) fn resize_tick(
    st: &mut StreamState,
    window: &mut sdl3::video::Window,
    persist: &mut dyn FnMut(u32, u32),
    render_scale: f64,
    max_dim: u32,
) {
    let Some(c) = &st.connector else {
        return; // not connected yet — the pending stamp survives until we are
    };
    let m = c.mode();
    // × the render scale so a resize under Match-window targets the same supersampled
    // space the live mode is in. resize_decision re-normalizes idempotently.
    let (pw, ph) = window.size_in_pixels();
    let pixel_size = punktfunk_core::render_scale::apply(pw, ph, render_scale, max_dim);
    match resize_decision(
        Instant::now(),
        &mut st.resize_pending,
        st.resize_sent_at,
        st.resize_requested,
        (m.width, m.height),
        pixel_size,
    ) {
        ResizeAction::Wait => {}
        ResizeAction::Settled(target) => {
            // Persist the window's logical size for the next launch even when no request
            // goes out (e.g. resized back to the streamed size).
            let (lw, lh) = window.size();
            persist(lw, lh);
            let Some((w, h)) = target else { return };
            tracing::info!(w, h, "window resized — requesting mode switch");
            if c.request_mode(Mode {
                width: w,
                height: h,
                refresh_hz: m.refresh_hz,
            })
            .is_err()
            {
                tracing::warn!("mode-switch request dropped — control channel closed");
            }
            st.resize_requested = Some((w, h));
            st.resize_sent_at = Some(Instant::now());
            // Scrim + spinner until a frame at this size lands: the live drag stays
            // sharp; only the host's rebuild gap is covered.
            st.resize_overlay.steering(w, h, Instant::now());
        }
    }
}

/// What one [`resize_decision`] tick decided.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ResizeAction {
    /// Nothing to do yet — the pending stamp is kept so a later tick retries.
    Wait,
    /// Debounce settled (caller persists the window size). `None` when the size needs no
    /// switch (equal to the streamed mode, or this exact size was already requested).
    Settled(Option<(u32, u32)>),
}

/// Debounce to resize-end, ≥ 1 s between requests, physical pixels even-floored and
/// clamped ≥ 320×200, skip when equal to the streamed mode, each distinct size at most
/// once (covers rejected sizes and host-side rollbacks).
pub(super) fn resize_decision(
    now: Instant,
    pending: &mut Option<Instant>,
    sent_at: Option<Instant>,
    requested: Option<(u32, u32)>,
    current: (u32, u32),
    pixel_size: (u32, u32),
) -> ResizeAction {
    const DEBOUNCE: Duration = Duration::from_millis(400);
    const SPACING: Duration = Duration::from_secs(1);
    let Some(since) = *pending else {
        return ResizeAction::Wait;
    };
    if now.duration_since(since) < DEBOUNCE {
        return ResizeAction::Wait;
    }
    if sent_at.is_some_and(|at| now.duration_since(at) < SPACING) {
        return ResizeAction::Wait; // keep the pending stamp — a later tick retries
    }
    *pending = None;
    let target = ((pixel_size.0 & !1).max(320), (pixel_size.1 & !1).max(200));
    if current == target || requested == Some(target) {
        return ResizeAction::Settled(None);
    }
    ResizeAction::Settled(Some(target))
}

/// Resize-in-progress overlay. A mid-stream Match-window switch takes the host a rebuild
/// of virtual display + encoder; the first new-mode frame is an IDR the decoder re-inits
/// on. A scrim + spinner from request until the sharp new-resolution frame is on screen.
///
/// Driven by signals the presenter already has (no new protocol):
/// * START — [`resize_tick`] reports the size it just requested (`steering`).
/// * END — decode reports each frame's dimensions; when they reach the target
///   (`decoded`). The accepted-switch ack alone cannot end it: the ack round-trips in
///   milliseconds, ahead of the host's rebuild.
/// * TIMEOUT — a switch that never delivers the exact target (reject, cap, or
///   corrective ack); `tick` clears it after [`ResizeIndicator::TIMEOUT`].
///
/// Pure + clock-injected so the transition logic is unit-tested without a live session.
#[derive(Default)]
pub(super) struct ResizeIndicator {
    /// Size the follower is steering toward — `Some` ⇔ show the scrim + spinner.
    target: Option<(u32, u32)>,
    /// When the current active span began — the timeout is measured from here.
    since: Option<Instant>,
}

impl ResizeIndicator {
    /// How long to keep the overlay up if the target frame never arrives.
    const TIMEOUT: Duration = Duration::from_millis(2500);

    pub(super) fn active(&self) -> bool {
        self.target.is_some()
    }

    /// A switch to `w`×`h` was just requested. The timeout re-arms only when the target
    /// actually changes, so a drag through several sizes never trips it mid-gesture.
    pub(super) fn steering(&mut self, w: u32, h: u32, now: Instant) {
        if self.target != Some((w, h)) {
            self.since = Some(now);
        }
        self.target = Some((w, h));
    }

    /// A decoded frame at `w`×`h`. Clears once it matches the steered target.
    pub(super) fn decoded(&mut self, w: u32, h: u32) {
        if self.target == Some((w, h)) {
            self.target = None;
            self.since = None;
        }
    }

    /// Stop showing once [`TIMEOUT`](Self::TIMEOUT) has elapsed with no matching frame.
    pub(super) fn tick(&mut self, now: Instant) {
        if self
            .since
            .is_some_and(|s| now.duration_since(s) >= Self::TIMEOUT)
        {
            self.target = None;
            self.since = None;
        }
    }
}

/// The ring's session facts for this frame.
pub(super) fn ring_facts(
    st: &StreamState,
    opts: &SessionOpts,
    stats: StatsVerbosity,
    mic_muted: bool,
    ring_opener: Option<u8>,
) -> RingFacts {
    let c = st.connector.as_ref().expect("filtered on connector");
    let m = c.mode();
    let target = pad_mouse_target(c, ring_opener);
    RingFacts {
        overlay_actions: opts.overlay_actions.clone(),
        touch_mode: st
            .capture
            .as_ref()
            .map_or(TouchMode::Trackpad, Capture::touch_mode)
            .as_name()
            .into(),
        invert_scroll: c.invert_scroll(),
        host_accepts_touch: c.host_caps2() & punktfunk_core::quic::HOST_CAP2_TOUCH != 0,
        stats_tier: stats.label().into(),
        // The mic control answers `toggle` with `None` when no uplink runs; a session
        // with a mic is one whose settings asked for it.
        mic_available: st.params.mic_enabled,
        mic_muted,
        pad_mouse_target: target,
        pad_mouse_on: target != 0 && c.pad_mouse() & target == target,
        audio_mute: c.audio_mute(),
        pointer_granted: c.access_grants() & punktfunk_core::quic::GRANT_POINTER != 0,
        mode: (m.width, m.height, m.refresh_hz),
        native_mode: st.native_mode,
        addr: st.params.host.clone(),
        mgmt_port: c.mgmt_port(),
        fp_hex: st.fp_hex.clone(),
        host_name: opts.window_title.clone(),
    }
}

/// Pads the controller-mouse toggle acts on: the pad that opened the ring, else every live pad.
pub(super) fn pad_mouse_target(c: &NativeClient, ring_opener: Option<u8>) -> u16 {
    ring_opener.map_or_else(
        || c.live_pads(),
        |pad| 1u16.checked_shl(pad.into()).unwrap_or(0),
    )
}

/// All target pads in controller mouse go back to passthrough; otherwise they all switch.
pub(super) fn toggle_pad_mouse(c: &NativeClient, ring_opener: Option<u8>) {
    let target = pad_mouse_target(c, ring_opener);
    let on = c.pad_mouse();
    let next = if on & target == target {
        on & !target
    } else {
        on | target
    };
    if let Err(e) = c.set_pad_mouse(next) {
        tracing::warn!(error = %e, "ring: controller mouse");
    }
}

impl Shell {
    /// Browse mode's console: menu events while no stream is engaged, and the action the
    /// console took. `Break` is the console's Quit.
    pub(super) fn browse_tick(
        &mut self,
        stream: &mut Option<StreamState>,
        on_action: &mut OnAction<'_>,
    ) -> ControlFlow<Outcome> {
        // Menu events flow while no stream is engaged — including a connect in
        // flight, so B can cancel the dial. Once attached, the worker forwards raw
        // input instead.
        if stream.as_ref().is_none_or(|s| s.connector.is_none()) {
            while let Ok(ev) = self.menu_rx.try_recv() {
                if let Some(o) = self.overlay.as_mut() {
                    if let Some(pulse) = o.handle_menu(ev) {
                        self.gamepad.menu_rumble(pulse);
                    }
                }
            }
        }
        if let Some(action) = self.overlay.as_mut().and_then(|o| o.take_action()) {
            match action {
                OverlayAction::CancelConnect => {
                    if let Some(st) = stream {
                        if st.connector.is_none() && !st.canceled {
                            tracing::info!("connect canceled from the console");
                            st.canceled = true;
                            st.handle.stop.store(true, Ordering::SeqCst);
                        }
                    }
                }
                // The console already toasted "Link copied"; a clipboard SDL refuses
                // is a log line, not a contradiction of the toast.
                OverlayAction::CopyText(text) => {
                    if let Err(e) = self.sdl_video.clipboard().set_clipboard_text(&text) {
                        tracing::warn!(error = %e, "copying to the clipboard");
                    }
                }
                action => {
                    let force_software = Arc::new(AtomicBool::new(false));
                    match on_action(
                        action,
                        &self.gamepad,
                        self.native,
                        window_display_hdr(&self.window),
                        force_software.clone(),
                        self.presenter.vulkan_decode(),
                    ) {
                        ActionOutcome::Handled => {}
                        ActionOutcome::Start(mut params) => {
                            if self.opts.match_window.is_some() {
                                apply_match_window(
                                    &mut params,
                                    &self.window,
                                    self.opts.render_scale,
                                    self.opts.render_scale_max_dim,
                                );
                            }
                            // Adopt the tier this launch resolved. The console outlives
                            // every stream. Not in `StreamState::new`: a codec-fallback
                            // retry rebuilds from a clone of these params and would snap
                            // the overlay back, undoing a chord the user had just made.
                            self.stats_verbosity = params.stats_verbosity;
                            // A live pump here would be detached by the assignment —
                            // `StreamState` has no `Drop`, so its thread would keep
                            // decoding onto the shared Vulkan device. Every other
                            // replacement site takes-and-shuts-down; so does this one.
                            if let Some(prev) = stream.take() {
                                tracing::warn!(
                                    "launch while a session was still attached — \
                                     stopping it first"
                                );
                                prev.shutdown();
                            }
                            *stream = Some(StreamState::new(
                                *params,
                                force_software,
                                self.sdl_events.event_sender(),
                                self.present_priority,
                                self.native.refresh_hz,
                            ));
                            if let Some(o) = self.overlay.as_mut() {
                                o.session_phase(SessionPhase::Connecting);
                            }
                        }
                        ActionOutcome::Quit => return ControlFlow::Break(Outcome::Ended(None)),
                    }
                }
            }
        }
        ControlFlow::Continue(())
    }
}

impl Shell {
    /// Run one ring command against the live session (stats tier, keyboard, system buttons
    /// and controller mouse are the loop's own and are handled at the call site).
    pub(super) fn ring_command(&mut self, cmd: RingCommand, st: &mut StreamState) {
        match cmd {
            RingCommand::EndStream => {
                st.request_quit();
                self.capture_off();
            }
            RingCommand::DisconnectLinger => {
                // Leave without the quit close code: the host lingers for a reconnect.
                if let Some(cap) = &mut st.capture {
                    cap.release(true);
                }
                st.handle.stop.store(true, Ordering::SeqCst);
                self.capture_off();
            }
            RingCommand::ToggleMic => {
                st.handle.mic.toggle();
            }
            RingCommand::ToggleScrollInvert => {
                if let Some(c) = st
                    .connector
                    .as_ref()
                    .filter(|c| c.access_grants() & punktfunk_core::quic::GRANT_POINTER != 0)
                {
                    c.set_invert_scroll(!c.invert_scroll());
                }
            }
            RingCommand::CycleTouchMode => {
                let accepts_touch = st
                    .connector
                    .as_ref()
                    .is_some_and(|c| c.host_caps2() & punktfunk_core::quic::HOST_CAP2_TOUCH != 0);
                if let Some(cap) = &mut st.capture {
                    let next = match (cap.touch_mode(), accepts_touch) {
                        (TouchMode::Trackpad, _) => TouchMode::Pointer,
                        (TouchMode::Pointer, true) => TouchMode::Touch,
                        (TouchMode::Pointer, false) | (TouchMode::Touch, _) => TouchMode::Off,
                        (TouchMode::Off, _) => TouchMode::Trackpad,
                    };
                    cap.set_touch_mode(next);
                }
            }
            RingCommand::RequestMode {
                width,
                height,
                refresh_hz,
            } => {
                if let Some(c) = &st.connector {
                    if let Err(e) = c.request_mode(punktfunk_core::config::Mode {
                        width,
                        height,
                        refresh_hz,
                    }) {
                        tracing::warn!(error = %e, "ring: mode request");
                    }
                }
            }
            RingCommand::Shortcut(keys) => {
                let vks: Vec<u8> = keys
                    .iter()
                    .filter_map(|k| pf_client_core::overlay_actions::key_vk(k))
                    .collect();
                if vks.len() == keys.len() {
                    if let Some(cap) = &mut st.capture {
                        cap.send_chord(&vks);
                    }
                }
            }
            RingCommand::CycleStats
            | RingCommand::Keyboard
            | RingCommand::TapButton(_)
            | RingCommand::TogglePadMouse
            | RingCommand::ToggleStreamMute => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_decision_follows_the_d2_discipline() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        let mut pending = None;
        assert_eq!(
            resize_decision(t0, &mut pending, None, None, (1280, 720), (1000, 600)),
            ResizeAction::Wait
        );

        // Still debouncing → wait, pending kept.
        let mut pending = Some(t0);
        assert_eq!(
            resize_decision(
                t0 + ms(399),
                &mut pending,
                None,
                None,
                (1280, 720),
                (1000, 600)
            ),
            ResizeAction::Wait
        );
        assert!(pending.is_some(), "pending survives the wait");

        // Debounce settled → request the even-floored, clamped pixel size.
        assert_eq!(
            resize_decision(
                t0 + ms(400),
                &mut pending,
                None,
                None,
                (1280, 720),
                (1001, 601)
            ),
            ResizeAction::Settled(Some((1000, 600))),
            "odd pixels floor to even"
        );
        assert!(pending.is_none(), "pending consumed");

        // Spacing: a request went out < 1 s ago → wait without dropping the pending
        // stamp, so a later tick retries.
        let mut pending = Some(t0);
        assert_eq!(
            resize_decision(
                t0 + ms(900),
                &mut pending,
                Some(t0),
                Some((1000, 600)),
                (1280, 720),
                (800, 500)
            ),
            ResizeAction::Wait
        );
        assert!(pending.is_some());
        assert_eq!(
            resize_decision(
                t0 + ms(1000),
                &mut pending,
                Some(t0),
                Some((1000, 600)),
                (1280, 720),
                (800, 500)
            ),
            ResizeAction::Settled(Some((800, 500)))
        );

        // Equal to the streamed mode → settle (persist) but no request.
        let mut pending = Some(t0);
        assert_eq!(
            resize_decision(
                t0 + ms(400),
                &mut pending,
                None,
                None,
                (1280, 720),
                (1280, 720)
            ),
            ResizeAction::Settled(None)
        );

        // A size already requested once (rejected, or rolled back host-side) is never
        // re-asked — no request → rollback → request loop.
        let mut pending = Some(t0);
        assert_eq!(
            resize_decision(
                t0 + ms(400),
                &mut pending,
                None,
                Some((1000, 600)),
                (1280, 720),
                (1000, 600)
            ),
            ResizeAction::Settled(None)
        );

        // Tiny windows clamp to the host's floor.
        let mut pending = Some(t0);
        assert_eq!(
            resize_decision(
                t0 + ms(400),
                &mut pending,
                None,
                None,
                (1280, 720),
                (100, 80)
            ),
            ResizeAction::Settled(Some((320, 200)))
        );
    }

    #[test]
    fn resize_indicator_shows_until_the_target_frame_or_timeout() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        let mut ind = ResizeIndicator::default();
        assert!(!ind.active());

        ind.steering(1000, 600, t0);
        assert!(ind.active());

        // A stale old-mode frame still draining does not lift it.
        ind.decoded(1280, 720);
        assert!(ind.active(), "an off-target frame keeps the scrim up");

        ind.decoded(1000, 600);
        assert!(!ind.active(), "the target frame lifts the scrim");
        ind.tick(t0 + ms(10_000)); // a late tick after clearing is inert
        assert!(!ind.active());

        // A switch whose target frame never arrives (rejected / host-capped) times out.
        let mut ind = ResizeIndicator::default();
        ind.steering(1000, 600, t0);
        ind.tick(t0 + ResizeIndicator::TIMEOUT - ms(1));
        assert!(ind.active(), "still within the timeout window");
        ind.tick(t0 + ResizeIndicator::TIMEOUT);
        assert!(!ind.active(), "timeout lifts a switch that never delivered");
    }

    #[test]
    fn resize_indicator_retargets_and_rearms_the_timeout_mid_drag() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        // A drag through sizes re-arms the timeout, so a slow gesture never trips it.
        let mut ind = ResizeIndicator::default();
        ind.steering(1000, 600, t0);
        let near = t0 + ResizeIndicator::TIMEOUT - ms(1);
        ind.steering(1200, 700, near); // new target → timeout re-armed from `near`
        ind.tick(t0 + ResizeIndicator::TIMEOUT + ms(1)); // past A's window, within B's
        assert!(
            ind.active(),
            "retarget re-armed the timeout — no mid-drag flicker"
        );

        // Re-steering the same size does not re-arm (a repeated identical request cannot
        // hold the scrim open forever).
        let mut ind = ResizeIndicator::default();
        ind.steering(1000, 600, t0);
        ind.steering(1000, 600, t0 + ms(500)); // same target, later — `since` unchanged
        ind.tick(t0 + ResizeIndicator::TIMEOUT);
        assert!(
            !ind.active(),
            "an unchanged target keeps the original timeout"
        );
    }
}
