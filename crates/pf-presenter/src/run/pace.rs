//! Video pacing: frame intake, the latch clock, the glass gate, and the 1 Hz window.

use super::*;

impl StreamState {
    /// The presenter found `on` busy for `image`: keep it for the next pass and wake
    /// soon. Newest-wins drops it if a fresher frame has landed meanwhile.
    pub(super) fn hold_busy(
        &mut self,
        on: crate::vk::BusyOn,
        image: Option<DecodedImage>,
        pts_ns: u64,
        decoded_ns: u64,
        due_ns: i64,
    ) {
        if let Some(image) = image {
            self.store.put_back(Paced {
                frame: DecodedFrame {
                    pts_ns,
                    decoded_ns,
                    image,
                },
                due_ns,
            });
        }
        self.win_busy[on as usize] += 1;
        self.busy_on = on;
        self.busy_retry = true;
    }

    /// Smoothness with a frame in hand sleeps only to the pass that can still serve it;
    /// everything else uses a 15 ms housekeeping tick. Must stay the present decision's
    /// mirror — a rule changed on one side oversleeps a smooth stream past its due time.
    pub(super) fn wake_timeout(&self) -> Duration {
        const TICK: Duration = Duration::from_millis(15);
        if self.busy_retry {
            // The fence wait inside the presenter is the pace; only a full swapchain
            // needs a refresh to pass.
            return match self.busy_on {
                crate::vk::BusyOn::Fence => Duration::ZERO,
                crate::vk::BusyOn::Acquire => Duration::from_millis(1),
            };
        }
        if !self.store.is_smoothing() {
            return TICK;
        }
        let Some(p) = self.store.front() else {
            return TICK;
        };
        // Free-running presents at the due time. Snapping presents once the aimed slot
        // is the next one still reachable (one period minus submit lead). Before the
        // first on-glass stamp there is no grid; `next_slot_after` answers "one period
        // from now" — mirror that or opening frames wait a refresh they never owed.
        let lead_ns = self.clock.period_ns() as i64 + self.margin_ns as i64;
        let wake_ns = if self.pacer.free_running() {
            p.due_ns
        } else if self.clock.anchor_ns() == 0 {
            p.due_ns - lead_ns
        } else {
            self.clock.next_slot_after(p.due_ns.max(0) as u64) as i64 - lead_ns
        };
        Duration::from_nanos(wake_ns.saturating_sub(session::now_ns() as i64).max(0) as u64)
            .clamp(Duration::from_millis(1), TICK)
    }
}

impl StreamState {
    /// Re-seed the latch grid, VRR verdict and pacer anchor from the window's current
    /// display mode. Re-anchoring costs one frame; measured jitter survives, because
    /// that describes the link.
    pub(super) fn relearn_grid(&mut self, window: &sdl3::video::Window) {
        let hz = window
            .get_display()
            .and_then(|d| d.get_mode())
            .map(|m| m.refresh_rate.round().max(0.0) as u32)
            .unwrap_or(0);
        if hz > 0 {
            self.clock = LatchClock::new(hz);
            self.mode_period_ns = 1_000_000_000 / u64::from(hz);
        }
        self.cadence.reset();
        self.pacer.reset();
        // The slot margin was sized by the old panel's misses.
        self.margin_ns = 0;
        self.win_misses = 0;
        tracing::info!(
            refresh_hz = hz,
            "display changed — relearning the latch grid"
        );
    }
}

/// One frame at `refresh_hz`, in ns — the source's nominal interval, and the cadence
/// cushion's ceiling.
///
/// The negotiated stream mode's refresh is the only source-rate signal a client has.
/// Measured fps sags when the transport is struggling, which is when a ceiling
/// derived from it would license a bigger hold.
///
/// `0` = "native", which the host resolves to this client's reported display rate.
/// Neither known falls back to 60 Hz, the same last resort [`native_mode`]'s caller takes.
pub(super) fn frame_interval_ns(refresh_hz: u32, fallback_hz: u32) -> i64 {
    let hz = match (refresh_hz, fallback_hz) {
        (0, 0) => 60,
        (0, f) => f,
        (r, _) => r,
    };
    1_000_000_000 / i64::from(hz)
}

/// Whether a present error is `VK_ERROR_DEVICE_LOST` in its chain. A lost device is
/// unrecoverable by spec — every object on it is dead, and demote-to-software would
/// rebuild the decoder against that same dead device. Fail the session and let the
/// shell relaunch.
pub(super) fn device_lost(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<ash::vk::Result>() == Some(&ash::vk::Result::ERROR_DEVICE_LOST))
}

/// Overlay changes no present has carried to the glass yet. A still host desktop sends
/// no frames, so an opened ring or a new OSD line would stay invisible while the ring
/// holds the pad, and the menu would read as frozen.
#[derive(Default)]
pub(super) struct OverlayDamage {
    image: Option<ash::vk::Image>,
    dirty: bool,
    video_at: Option<Instant>,
}

impl OverlayDamage {
    /// Video quiet this long hands the overlay its own presents: longer than a live
    /// stream's frame gap, short enough that the ring opens without a visible lag.
    const VIDEO_QUIET: Duration = Duration::from_millis(100);

    /// Once per pass, after `Overlay::frame`. A re-render lands in the other ring slot.
    pub(super) fn rendered(&mut self, image: Option<ash::vk::Image>) {
        self.dirty |= image != self.image;
        self.image = image;
    }

    /// A video present composited the current overlay.
    pub(super) fn video_presented(&mut self, now: Instant) {
        self.dirty = false;
        self.video_at = Some(now);
    }

    /// Browsing: the overlay changed since the last present.
    pub(super) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// The overlay changed over a picture that has gone still: present it once.
    pub(super) fn take_due(&mut self, now: Instant) -> bool {
        let still = self
            .video_at
            .is_some_and(|t| now.duration_since(t) >= Self::VIDEO_QUIET);
        let due = self.dirty && still;
        self.dirty &= !due;
        due
    }
}

/// Advance the stats-overlay tier and re-render the OSD immediately from the last
/// window (waiting for the next Stats event would lag the trigger by up to 1 s).
pub(super) fn bump_stats_tier(verbosity: &mut StatsVerbosity, stream: &mut Option<StreamState>) {
    *verbosity = verbosity.next();
    if let Some(st) = stream {
        render_osd(st, *verbosity);
    }
}

/// The presenter's own counters for one window: what the `present:` line reports.
pub(super) struct PresentCounters {
    pub(super) mode: &'static str,
    pub(super) vrr: Cadence,
    pub(super) smoothing: bool,
    pub(super) q_drop: u32,
    pub(super) q_dry: u32,
    pub(super) gated: u32,
    pub(super) forced: u32,
}

/// Close the overlay window: the connector's snapshot plus what only the presenter knows,
/// then the OSD and the stdout lines. Returns the display split's p50s (ms) for the log.
pub(super) fn close_window(
    st: &mut StreamState,
    presenter: &Presenter,
    present: &PresentCounters,
    replaced: u32,
    tier: StatsVerbosity,
) -> (f32, f32) {
    let Some(c) = st.connector.clone() else {
        return (0.0, 0.0);
    };
    // Replaced before display, or dropped from a full smoothing queue: decoded, never shown.
    c.hud()
        .note_skipped(replaced.saturating_add(present.q_drop), 0);
    let mut snap = c.hud_snapshot();
    snap.decoder = st.facts.decoder.to_string();
    snap.hdr = hdr_shown(st.hdr, presenter.hdr_active(), st.hdr_untonemapped);
    snap.asked_444 = st.params.video_caps & punktfunk_core::quic::VIDEO_CAP_444 != 0;
    snap.preset = st.preset.clone();
    snap.on_glass = presenter.present_timing_active();
    let prev = std::mem::replace(&mut st.health_seen, st.facts.health);
    snap.extras = desktop_extras(present, st.facts.health, prev, session::codec_fallbacks());
    // The field bundle's per-second record, whatever the HUD tier: a report with no
    // stats line cannot say where its frames went.
    let text = hud::join(&hud::format(&snap, StatsVerbosity::Detailed, true), " | ");
    tracing::info!(target: "stats", "{text}");
    if tier != StatsVerbosity::Off {
        emit(SessionLine::Stats {
            text: &text,
            snap: &snap,
        });
    }
    let split = (
        snap.pace.p50_us as f32 / 1000.0,
        snap.latch.p50_us as f32 / 1000.0,
    );
    st.last_snap = Some(snap);
    render_osd(st, tier);
    split
}

/// Re-render the OSD from the last closed window at `tier`.
pub(super) fn render_osd(st: &mut StreamState, tier: StatsVerbosity) {
    st.osd = match &st.last_snap {
        Some(s) => hud::format(s, tier, st.params.advanced_stats),
        None => Vec::new(),
    };
}

/// How the stream reaches the screen. No present arm sets `untonemapped` today; the tag
/// stays so a lane that bypasses CSC can say so rather than claim a tone-map.
pub(super) fn hdr_shown(stream_hdr: bool, display_hdr: bool, untonemapped: bool) -> hud::Hdr {
    match (stream_hdr, display_hdr, untonemapped) {
        (false, ..) => hud::Hdr::Sdr,
        (true, true, _) => hud::Hdr::Hdr,
        (true, false, true) => hud::Hdr::Untonemapped,
        (true, false, false) => hud::Hdr::ToneMapped,
    }
}

/// Lines only the desktop measures: the live present path, decode integrity over this window
/// (`health` against `prev`), and this process's codec fallbacks.
pub(super) fn desktop_extras(
    p: &PresentCounters,
    health: Option<DecodeHealth>,
    prev: Option<DecodeHealth>,
    codec_fallbacks: u64,
) -> Vec<hud::Extra> {
    let mut out = Vec::new();
    if !p.mode.is_empty() {
        let mut t = format!("present: {}", p.mode);
        // Only once measured: an unproven "vrr no" would be a claim, not a reading.
        if p.vrr != Cadence::Unknown {
            t.push_str(&format!(" · vrr {}", p.vrr.label()));
        }
        if p.smoothing {
            t.push_str(" · smoothing");
        }
        for (name, n) in [
            ("qdrop", p.q_drop),
            ("qdry", p.q_dry),
            ("gated", p.gated),
            ("forced", p.forced),
        ] {
            if n > 0 {
                t.push_str(&format!(" · {name} {n}"));
            }
        }
        out.push(hud::Extra {
            text: t,
            tier: StatsVerbosity::Detailed,
            advanced_only: false,
            role: hud::Role::Muted,
        });
    }
    // A lane that cannot see damage says nothing; one that looked and saw none also says
    // nothing; one with half its detectors says so every window.
    if let Some(h) = health {
        let base = prev.unwrap_or_default();
        let damaged = h.damaged.saturating_sub(base.damaged);
        let refused = h.refused.saturating_sub(base.refused);
        let failed = h.failed.saturating_sub(base.failed);
        let mut parts = Vec::new();
        if damaged > 0 {
            parts.push(format!("damaged {damaged}"));
        }
        if refused > 0 {
            parts.push(format!("refused {refused}"));
        }
        if failed > 0 {
            parts.push(format!("driver-failed {failed}"));
        }
        if h.run > 0 {
            parts.push(format!("run {}", h.run));
        }
        // Session-cumulative: a 1 Hz sample of `run` misses the worst moment.
        if h.worst_run > h.run {
            parts.push(format!("worst run {}", h.worst_run));
        }
        if !h.status_queries {
            parts.push("no driver status".into());
        }
        if !parts.is_empty() {
            let hurt = damaged + refused + failed > 0 || h.run > 0;
            out.push(hud::Extra {
                text: format!("integrity: {}", parts.join(" · ")),
                tier: StatsVerbosity::Detailed,
                advanced_only: true,
                role: if hurt {
                    hud::Role::Warn
                } else {
                    hud::Role::Muted
                },
            });
        }
    }
    if codec_fallbacks > 0 {
        out.push(hud::Extra::detail(format!(
            "codec_fallbacks {codec_fallbacks}"
        )));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_damage_presents_a_changed_overlay_only_over_a_still_picture() {
        use ash::vk::Handle as _;
        let (a, b) = (ash::vk::Image::from_raw(1), ash::vk::Image::from_raw(2));
        let quiet = OverlayDamage::VIDEO_QUIET;
        let t0 = Instant::now();
        let mut d = OverlayDamage::default();
        // No video yet: nothing to present over.
        d.rendered(Some(a));
        assert!(!d.take_due(t0 + quiet));
        // Live video carries the change.
        d.video_presented(t0);
        d.rendered(Some(b));
        assert!(!d.take_due(t0 + quiet / 2));
        // The picture went still: the change presents once.
        assert!(d.take_due(t0 + quiet));
        assert!(!d.take_due(t0 + quiet * 2));
        // An unchanged overlay does not; closing it does.
        d.rendered(Some(b));
        assert!(!d.take_due(t0 + quiet * 2));
        d.rendered(None);
        assert!(d.take_due(t0 + quiet * 2));
    }

    /// Browsing presents each overlay change once; an idle console hands back the same image.
    #[test]
    fn overlay_damage_browse_presents_only_a_changed_overlay() {
        use ash::vk::Handle as _;
        let (a, b) = (ash::vk::Image::from_raw(1), ash::vk::Image::from_raw(2));
        let mut d = OverlayDamage::default();
        d.rendered(Some(a));
        assert!(d.take_dirty());
        d.rendered(Some(a));
        assert!(!d.take_dirty());
        d.rendered(Some(b));
        assert!(d.take_dirty());
    }

    /// Cadence cushion is bounded by the source's frame interval, not the panel's.
    #[test]
    fn the_cadence_interval_comes_from_the_stream_mode_not_the_panel() {
        assert_eq!(frame_interval_ns(120, 60), 8_333_333);
        assert_eq!(frame_interval_ns(60, 165), 16_666_666);
        // A `0 = native` request is resolved by the host to the display this client
        // reported, so that display's rate is what it will produce.
        assert_eq!(frame_interval_ns(0, 165), 6_060_606);
        // Neither known: 60 Hz, never an unbounded ceiling.
        assert_eq!(frame_interval_ns(0, 0), 16_666_666);
    }

    fn counters() -> PresentCounters {
        PresentCounters {
            mode: "fifo",
            vrr: Cadence::Unknown,
            smoothing: false,
            q_drop: 0,
            q_dry: 0,
            gated: 0,
            forced: 0,
        }
    }

    /// The present line names the live mode; counters show only when non-zero, and VRR only
    /// once measured.
    #[test]
    fn the_present_line_says_only_what_moved() {
        let quiet = desktop_extras(&counters(), None, None, 0);
        assert_eq!(quiet.len(), 1);
        assert_eq!(quiet[0].text, "present: fifo");
        assert!(
            !quiet[0].advanced_only,
            "Standard Detailed shows the present path too"
        );
        let busy = PresentCounters {
            vrr: Cadence::Variable,
            smoothing: true,
            q_drop: 2,
            q_dry: 1,
            gated: 7,
            forced: 1,
            ..counters()
        };
        assert_eq!(
            desktop_extras(&busy, None, None, 0)[0].text,
            "present: fifo · vrr yes · smoothing · qdrop 2 · qdry 1 · gated 7 · forced 1"
        );
        let no_mode = PresentCounters {
            mode: "",
            ..counters()
        };
        assert!(desktop_extras(&no_mode, None, None, 0).is_empty());
        assert_eq!(
            desktop_extras(&no_mode, None, None, 2)[0].text,
            "codec_fallbacks 2"
        );
    }

    /// Integrity tells three quiet states apart: a lane that cannot see damage, one that
    /// looked and saw none, and one with only half its detectors.
    #[test]
    fn the_integrity_line_distinguishes_clean_from_unmeasurable() {
        let no_mode = PresentCounters {
            mode: "",
            ..counters()
        };
        let line = |now: DecodeHealth, prev: Option<DecodeHealth>| {
            desktop_extras(&no_mode, Some(now), prev, 0)
                .into_iter()
                .map(|e| e.text)
                .next()
        };
        assert!(desktop_extras(&no_mode, None, None, 0).is_empty());
        let clean = DecodeHealth {
            status_queries: true,
            ..DecodeHealth::default()
        };
        assert_eq!(line(clean, None), None);
        let radv = DecodeHealth {
            status_queries: false,
            ..clean
        };
        assert_eq!(
            line(radv, None).as_deref(),
            Some("integrity: no driver status")
        );
        let damaged = DecodeHealth {
            damaged: 4,
            failed: 2,
            run: 3,
            worst_run: 3,
            ..clean
        };
        assert_eq!(
            line(damaged, None).as_deref(),
            Some("integrity: damaged 4 · driver-failed 2 · run 3")
        );
        let recovered_hard = DecodeHealth {
            damaged: 4,
            worst_run: 40,
            ..clean
        };
        assert_eq!(
            line(recovered_hard, None).as_deref(),
            Some("integrity: damaged 4 · worst run 40")
        );
        let refusing = DecodeHealth {
            refused: 60,
            run: 60,
            worst_run: 60,
            ..clean
        };
        assert_eq!(
            line(refusing, None).as_deref(),
            Some("integrity: refused 60 · run 60")
        );
        // Windowed against the last window's cumulative counters.
        let later = DecodeHealth {
            damaged: 6,
            ..clean
        };
        let before = DecodeHealth {
            damaged: 4,
            ..clean
        };
        assert_eq!(
            line(later, Some(before)).as_deref(),
            Some("integrity: damaged 2")
        );
        let e = desktop_extras(&no_mode, Some(damaged), None, 0);
        assert!(e[0].advanced_only && e[0].role == hud::Role::Warn);
    }

    #[test]
    fn hdr_tag_follows_the_swapchain() {
        assert_eq!(hdr_shown(false, true, false), hud::Hdr::Sdr);
        assert_eq!(hdr_shown(true, true, false), hud::Hdr::Hdr);
        assert_eq!(hdr_shown(true, false, false), hud::Hdr::ToneMapped);
        assert_eq!(hdr_shown(true, false, true), hud::Hdr::Untonemapped);
    }
}
