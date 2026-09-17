//! The scenario table, and the field calibration that accepts the simulator.
//!
//! C1–C5 reproduce behaviours from the 09-16/17 traces and Klos54's 70
//! sessions with the controller untouched; the rest have no field trace and
//! simply record what today's controller does. Every tuned number carries the
//! reading it came from.

use super::client::{ClientCfg, DecodeCfg};
use super::host::{ContentPhase, HostCfg};
use super::link::LinkCfg;
use super::{run, Scenario, SessionCfg};
use crate::abr::probe::wall_ceiling_kbps;
use crate::abr::stream_ceiling_kbps;
use crate::quic::{CODEC_H264, CODEC_HEVC};

/// `PUNKTFUNK_ABR_PROBE_KBPS` on the webOS client: the burst target it pins
/// so a 2 Gbps default does not take the picture with it.
const WEBOS_PROBE_KBPS: u32 = 320_000;

/// Display, capture and encoder bring-up. Windows measured
/// `punch_done+1848 … first_packet+4815`; the ramp lives in that gap.
const BRINGUP_MS: u64 = 2_500;

/// The same scenario against a host that serves the bring-up ramp: video
/// waits for the pipeline, and the client measures the link in that gap.
fn with_ramp(mut sc: Scenario) -> Scenario {
    for s in &mut sc.sessions {
        if s.host.ramp {
            continue; // already a ramp scenario, with a bring-up of its own
        }
        s.host.ramp = true;
        s.host.bringup_ms = s.join_ms + BRINGUP_MS;
        s.client.ramp = true;
        // An injected ceiling replayed a host that paused video for its
        // burst. The ramp is that measurement now, so it runs instead.
        if s.client.ceiling_at.take().is_some() {
            s.client.probe = true;
        }
    }
    sc
}

/// A calibration kept as it was recorded: an old host, no ramp, video from
/// the first millisecond.
fn legacy(mut sc: Scenario, name: &'static str) -> Scenario {
    sc.name = name;
    sc
}

/// What a session that respects a measured wall holds here: the ceiling the
/// ramp leaves under `measured_kbps`, split between the sessions sharing the
/// path and bounded by the stream's own shape.
///
/// `to90_s` is read against this. The link's nominal capacity is the wrong
/// yardstick — a session parked under its wall by design would read as one
/// that never arrived, which is the program's best rows reporting as its
/// worst. Derived from the ceiling rule, so both move together.
///
/// `measured_kbps` is what the ramp reads on this link, which is the capacity
/// unless a deep queue stretches the step that trips the wall.
fn wall_respecting_kbps(measured_kbps: u32, sessions: u32, stream_cap_kbps: u32) -> u32 {
    wall_ceiling_kbps(measured_kbps / sessions.max(1)).min(stream_cap_kbps)
}

/// 4K165 HEVC 8-bit — the G5 sessions' mode.
fn cap_4k165() -> u32 {
    stream_ceiling_kbps(3840, 2160, 165, CODEC_HEVC, 8, 0)
}

/// 1080p30 HEVC — Klos54's mode.
fn cap_1080p30() -> u32 {
    stream_ceiling_kbps(1920, 1080, 30, CODEC_HEVC, 8, 0)
}

/// Content that fills its target and produces every frame.
fn full() -> Vec<ContentPhase> {
    vec![ContentPhase::default()]
}

/// The G5's own decode readings: 110–240 µs against a 6 060 µs budget.
fn g5_decode() -> DecodeCfg {
    DecodeCfg {
        base_us: 130,
        jitter_us: 110,
        knee_kbps: u32::MAX,
        us_per_mbps: 0,
    }
}

/// One Automatic session at 4K165 over the G5's Wi-Fi.
fn tv_session(
    start_kbps: u32,
    ceiling_at: Option<(u64, u32)>,
    content: Vec<ContentPhase>,
) -> SessionCfg {
    SessionCfg {
        join_ms: 0,
        host: HostCfg {
            fps: 165,
            audio_kbps: 512,
            encode_us: 3_550,
            encode_jitter_us: 300,
            content,
            ..HostCfg::default()
        },
        client: ClientCfg {
            start_kbps,
            refresh_hz: 165,
            stream_cap_kbps: cap_4k165(),
            audio_kbps: 512,
            decode: g5_decode(),
            // A scenario that injects the ceiling is replaying a host that
            // paused video for the burst; it runs no probe of its own, and
            // must hand the measurement over before the first window closes
            // — an unmeasured session climbs toward its stream shape.
            probe: ceiling_at.is_none(),
            ceiling_at,
            ..ClientCfg::default()
        },
    }
}

/// C1 — the G5's clean start, 19:55:46–19:56:08 on 09-16.
///
/// The content numbers come out of the trace. `active_pct` is
/// `123 × actual × 1.5 ÷ next` over its twelve decisions: 106–112 new-content
/// frames a window, so the source ran at ~145 of the session's 165 fps.
/// `fill_pct` is what each frame then spent of its allowance — 0.67 over the
/// first seconds and 0.84 by the top, so two motion phases.
pub(super) fn wifi_tv() -> Scenario {
    Scenario {
        name: "wifi_tv",
        seed: 0x7A_5100,
        duration_ms: 40_000,
        link: LinkCfg {
            capacity: vec![(0, 245_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            stall_every_ms: 0,
            ..LinkCfg::default()
        },
        sessions: vec![tv_session(
            20_000,
            Some((0, 171_294)),
            vec![
                ContentPhase {
                    until_ms: 10_000,
                    fill_pct: 68,
                    active_pct: 88,
                    ..ContentPhase::default()
                },
                ContentPhase {
                    fill_pct: 78,
                    active_pct: 88,
                    ..ContentPhase::default()
                },
            ],
        )],
        achievable_kbps: 171_294,
        blip_at_ms: None,
    }
}

/// C2 — the sawtooth: at the ceiling, one unrecoverable frame.
pub(super) fn wifi_good() -> Scenario {
    Scenario {
        name: "wifi_good",
        seed: 0x7A_5200,
        duration_ms: 70_000,
        link: LinkCfg {
            capacity: vec![(0, 400_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            stall_every_ms: 25_000,
            stall_ms: 100,
            ..LinkCfg::default()
        },
        sessions: vec![tv_session(
            171_294,
            Some((0, 171_294)),
            vec![ContentPhase {
                fill_pct: 80,
                active_pct: 88,
                ..ContentPhase::default()
            }],
        )],
        achievable_kbps: 171_294,
        blip_at_ms: Some(30_000),
    }
}

/// C6 — the 0.39 regression: the burst takes the picture with it.
///
/// The burst runs beside live video (#1146), so on a link it overdrives the
/// video frames beside it die. The client freezes and asks for a keyframe
/// every 100 ms until one lands; the host answers most asks with an
/// intra-refresh wave (`host173` 09-17 09:53–09:54: `keyframe_req=9 idr=2
/// rfi=8`), which does not unfreeze a client that lost its reference. The
/// asks that outlive the discarded tail window are the first thing the
/// controller judges, and four of them are severe.
pub(super) fn wifi_tv_probe_damage() -> Scenario {
    let mut s = tv_session(
        20_000,
        None,
        vec![
            ContentPhase {
                until_ms: 10_000,
                fill_pct: 68,
                active_pct: 88,
                ..ContentPhase::default()
            },
            ContentPhase {
                fill_pct: 78,
                active_pct: 88,
                ..ContentPhase::default()
            },
        ],
    );
    s.client.probe_target_kbps = Some(WEBOS_PROBE_KBPS);
    s.host.recovery_ms = 1_200;
    Scenario {
        name: "wifi_tv_probe_damage",
        seed: 0x7A_5E00,
        duration_ms: 60_000,
        link: LinkCfg {
            capacity: vec![(0, 245_000)],
            // A consumer AP's aggregation queue, not the 60 ms a switch
            // holds. Under 180 ms every ask lands in the discarded tail;
            // over 320 ms the queue swallows the whole burst and no frame
            // dies at all.
            buffer_ms: 250,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 168_000,
        blip_at_ms: None,
    }
}

/// C7 — the same burst, the quieter half of the regression: the freeze is
/// short enough that only two or three asks reach a judged window. Two is
/// under the severe bar but at `RECOVERY_KF_BAD`, so the window is bad, and
/// a bad window ends slow start for the session. Nothing cuts, so nothing in
/// any log marks it; the session simply crawls at +6 % for the rest of its
/// life.
///
/// The second field session of 09-17 (`host173` 10:03): probe complete
/// 10:03:20.3, then 25 955 · 27 578 · 29 302 · 31 134, no cut anywhere, and
/// 31 134 held for three and a half minutes.
pub(super) fn wifi_tv_probe_stalled() -> Scenario {
    let mut sc = wifi_tv_probe_damage();
    sc.name = "wifi_tv_probe_stalled";
    sc.seed = 0x7A_6500;
    sc.sessions[0].host.recovery_ms = 1_050;
    sc
}

/// C3 — one severe window inside the first 10 s, then the +6 % crawl.
pub(super) fn slow_start_spent() -> Scenario {
    Scenario {
        name: "slow_start_spent",
        seed: 0x7A_5300,
        duration_ms: 230_000,
        link: LinkCfg {
            capacity: vec![(0, 400_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![tv_session(
            20_000,
            None,
            vec![
                // Nine seconds of content that does not fill three quarters
                // of the target authorises no climb — the session was still
                // at 20 000 when the host log's severe window landed. 40 %
                // of the allowance, not 75: the 2-shard parity floor pads a
                // 4K165 frame at 20 Mbps by half again.
                ContentPhase {
                    until_ms: 9_000,
                    fill_pct: 40,
                    ..ContentPhase::default()
                },
                ContentPhase::default(),
            ],
        )],
        achievable_kbps: 171_294,
        blip_at_ms: Some(6_000),
    }
}

/// A host that refuses climbs for ten seconds mid-session, naming its encode
/// cadence. The link is fine and the encoder holds every rate it is at, so
/// nothing but the refusal stops the climb.
///
/// The refusal is the same short ack an encoder ceiling sends. Read as a
/// ceiling it costs a learned cap and a 12 s re-probe ladder on a limit that
/// clears on its own; read as cadence it costs the climbs inside the window
/// and one clean run.
pub(super) fn host_cadence_refusal() -> Scenario {
    let mut s = tv_session(20_000, None, full());
    s.client.probe_target_kbps = Some(WEBOS_PROBE_KBPS);
    s.host.cadence_refusal_ms = (8_000, 18_000);
    Scenario {
        name: "host_cadence_refusal",
        seed: 0x7A_7200,
        duration_ms: 90_000,
        link: LinkCfg {
            capacity: vec![(0, 245_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 168_000,
        blip_at_ms: None,
    }
}

/// C4 — .21's saturated GPU: encode 16.5–21 ms against a 6 060 µs budget.
///
/// Contention swings over seconds, so the window means swing with it — that
/// swing, not the level, is what a rolling minimum reads as a rise once our
/// own cut has cleared the healthy baseline.
pub(super) fn gpu_saturated() -> Scenario {
    let mut s = tv_session(40_000, None, full());
    s.host.encode_us = 3_550;
    s.host.encode_jitter_us = 300;
    s.host.loaded_encode_us = 16_500;
    s.host.loaded_from_ms = 12_000;
    s.host.encode_swing_us = 4_500;
    s.host.encode_swing_ms = 1_500;
    s.client.probe_target_kbps = Some(WEBOS_PROBE_KBPS);
    Scenario {
        name: "gpu_saturated",
        seed: 0x7A_5400,
        duration_ms: 110_000,
        link: LinkCfg {
            capacity: vec![(0, 245_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 168_000,
        blip_at_ms: None,
    }
}

/// An encoder whose time really is a function of the rate: past 60 Mbps
/// every extra megabit costs 40 µs against a 6 060 µs budget.
///
/// The notch is answered here, so it is kept and the next rise takes another:
/// the session must walk down to where the encoder keeps up instead of
/// standing the driver down.
pub(super) fn encoder_weak() -> Scenario {
    let mut s = tv_session(40_000, None, full());
    s.client.probe_target_kbps = Some(WEBOS_PROBE_KBPS);
    s.host.encode_knee_kbps = 40_000;
    s.host.encode_us_per_mbps = 200;
    Scenario {
        name: "encoder_weak",
        seed: 0x7A_7100,
        duration_ms: 110_000,
        link: LinkCfg {
            capacity: vec![(0, 245_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 168_000,
        blip_at_ms: None,
    }
}

/// One 1080p30 Automatic session over Klos54's WireGuard path.
fn wg_session() -> SessionCfg {
    SessionCfg {
        join_ms: 0,
        host: HostCfg {
            fps: 30,
            audio_kbps: 128,
            encode_us: 6_000,
            encode_jitter_us: 500,
            content: full(),
            ..HostCfg::default()
        },
        client: ClientCfg {
            start_kbps: 20_000,
            refresh_hz: 30,
            stream_cap_kbps: cap_1080p30(),
            audio_kbps: 128,
            decode: DecodeCfg {
                base_us: 4_000,
                jitter_us: 800,
                knee_kbps: u32::MAX,
                us_per_mbps: 0,
            },
            ..ClientCfg::default()
        },
    }
}

/// C5 — the tunnel: 10–18 Mbps behind a bloated queue.
///
/// Buffer, loss, delay and wander period are mid-range of #1131/#1228.
/// Capacity is 12 500 kbps because that is where his wall actually stood on
/// three separate nights (12.58 · 12.49 · 12.00 Mbps); ±30 % over three
/// minutes puts the session between 8.8 and 16.3.
pub(super) fn wan_wg_12(seed: u64, duration_ms: u64) -> Scenario {
    Scenario {
        name: "wan_wg_12",
        seed,
        duration_ms,
        link: LinkCfg {
            capacity: vec![(0, 12_500)],
            wander_pct: 30,
            wander_ms: 180_000,
            buffer_ms: 450,
            base_delay_ms: 10,
            loss_ppm: 7_000,
            ..LinkCfg::default()
        },
        sessions: vec![wg_session()],
        // The ramp reads 10 811 here, not 12 500: the step that trips the
        // wall drains through 450 ms of the queue it is measuring.
        achievable_kbps: wall_respecting_kbps(10_811, 1, cap_1080p30()),
        blip_at_ms: None,
    }
}

/// A tunnel that browns out for ten seconds after a clean run, then comes
/// back — the case a delivered-rate cut has to leave alone once it lands.
///
/// The clean run is what makes it: the rolling delay minimum still remembers
/// the uncongested floor, so the queue the overshoot built reads as a rise for
/// as long as it takes to empty. Every one of those windows would otherwise
/// be a second verdict on a rate that is already under the wall.
pub(super) fn wan_brownout() -> Scenario {
    Scenario {
        name: "wan_brownout",
        seed: 0x7A_6E00,
        duration_ms: 90_000,
        link: LinkCfg {
            capacity: vec![(0, 20_000), (30_000, 9_000), (60_000, 20_000)],
            buffer_ms: 2_000,
            base_delay_ms: 30,
            ..LinkCfg::default()
        },
        sessions: vec![wg_session()],
        achievable_kbps: wall_respecting_kbps(20_000, 1, cap_1080p30()),
        blip_at_ms: None,
    }
}

/// A wired session: the link is never the limit.
fn lan(name: &'static str, capacity_kbps: u32, refresh_hz: u32) -> Scenario {
    let cap = stream_ceiling_kbps(3840, 2160, refresh_hz, CODEC_HEVC, 8, 0);
    Scenario {
        name,
        seed: 0x7A_5600,
        duration_ms: 60_000,
        link: LinkCfg {
            capacity: vec![(0, capacity_kbps)],
            buffer_ms: 20,
            base_delay_ms: 1,
            ..LinkCfg::default()
        },
        sessions: vec![SessionCfg {
            join_ms: 0,
            host: HostCfg {
                fps: refresh_hz,
                audio_kbps: 512,
                content: full(),
                ..HostCfg::default()
            },
            client: ClientCfg {
                start_kbps: 20_000,
                refresh_hz,
                stream_cap_kbps: cap,
                audio_kbps: 512,
                ..ClientCfg::default()
            },
        }],
        achievable_kbps: wall_respecting_kbps(capacity_kbps, 1, cap),
        blip_at_ms: Some(30_000),
    }
}

pub(super) fn lan_10g() -> Scenario {
    lan("lan_10g", 10_000_000, 120)
}

pub(super) fn lan_1g() -> Scenario {
    lan("lan_1g", 1_000_000, 120)
}

/// A cell that moves between 2 and 50 Mbps with handover stalls.
pub(super) fn lte_variable() -> Scenario {
    let mut s = wg_session();
    s.client.start_kbps = 20_000;
    Scenario {
        name: "lte_variable",
        seed: 0x7A_5700,
        duration_ms: 180_000,
        link: LinkCfg {
            capacity: vec![
                (0, 30_000),
                (40_000, 8_000),
                (75_000, 50_000),
                (110_000, 2_500),
                (140_000, 18_000),
            ],
            wander_pct: 20,
            wander_ms: 20_000,
            buffer_ms: 250,
            base_delay_ms: 30,
            loss_ppm: 3_000,
            burst_in_ppm: 400,
            burst_out_ppm: 300_000,
            burst_shards: 14,
            stall_every_ms: 30_000,
            stall_ms: 250,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        // The trace's last and longest leg; the cell is elsewhere for half
        // the run, and one number cannot describe both.
        achievable_kbps: wall_respecting_kbps(18_000, 1, cap_1080p30()),
        blip_at_ms: None,
    }
}

/// Two sessions over one tunnel. `join_ms` decides which of the three
/// shared-path cases this is.
fn shared(name: &'static str, second_join_ms: u64, second_fixed: bool) -> Scenario {
    let mut first = wg_session();
    first.client.start_kbps = 20_000;
    let mut second = wg_session();
    second.join_ms = second_join_ms;
    second.client.automatic = !second_fixed;
    second.client.start_kbps = if second_fixed { 8_000 } else { 20_000 };
    Scenario {
        name,
        seed: 0x7A_5800,
        duration_ms: 150_000,
        link: LinkCfg {
            capacity: vec![(0, 18_000)],
            buffer_ms: 450,
            base_delay_ms: 10,
            loss_ppm: 5_000,
            ..LinkCfg::default()
        },
        sessions: vec![first, second],
        // Two sessions, one tunnel: half the path each.
        achievable_kbps: wall_respecting_kbps(18_000, 2, cap_1080p30()),
        blip_at_ms: None,
    }
}

pub(super) fn shared_two_auto() -> Scenario {
    shared("shared_two_auto", 0, false)
}

pub(super) fn shared_newcomer() -> Scenario {
    shared("shared_newcomer", 60_000, false)
}

pub(super) fn shared_fixed_plus_auto() -> Scenario {
    shared("shared_fixed_plus_auto", 0, true)
}

/// August's Phase 3 cases: a still desktop that starts moving, and a source
/// that never fills the wall-clock target.
pub(super) fn static_then_motion() -> Scenario {
    let mut s = tv_session(20_000, None, full());
    s.host.content = vec![
        ContentPhase {
            until_ms: 20_000,
            ..ContentPhase::default()
        },
        ContentPhase {
            until_ms: 40_000,
            idle: true,
            ..ContentPhase::default()
        },
        ContentPhase {
            cut_every_ms: 10_000,
            cut_pct: 500,
            ..ContentPhase::default()
        },
    ];
    Scenario {
        name: "static_then_motion",
        seed: 0x7A_5900,
        duration_ms: 70_000,
        link: LinkCfg {
            capacity: vec![(0, 400_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 171_294,
        blip_at_ms: None,
    }
}

pub(super) fn frame_driven_35fps() -> Scenario {
    let mut s = tv_session(
        20_000,
        None,
        vec![ContentPhase {
            active_pct: 21,
            ..ContentPhase::default()
        }],
    );
    s.host.fps = 165;
    Scenario {
        name: "frame_driven_35fps",
        seed: 0x7A_5A00,
        duration_ms: 70_000,
        link: LinkCfg {
            capacity: vec![(0, 400_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 171_294,
        blip_at_ms: None,
    }
}

/// A host that never flags idle repeats: every window is wall-clock.
pub(super) fn old_host() -> Scenario {
    let mut s = tv_session(20_000, None, full());
    s.host.marks_repeats = false;
    s.client.marks_repeats = false;
    s.host.content = vec![
        ContentPhase {
            until_ms: 25_000,
            ..ContentPhase::default()
        },
        ContentPhase {
            idle: true,
            ..ContentPhase::default()
        },
    ];
    Scenario {
        name: "old_host",
        seed: 0x7A_5B00,
        duration_ms: 60_000,
        link: LinkCfg {
            capacity: vec![(0, 400_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 171_294,
        blip_at_ms: None,
    }
}

/// Wi-Fi interference: bursts of loss parity still covers, so no frame dies
/// and the whole signal is the repaired share. `ppm` names it.
fn wifi_loss(name: &'static str, loss_ppm: u32) -> Scenario {
    let mut s = tv_session(
        20_000,
        None,
        vec![ContentPhase {
            fill_pct: 78,
            active_pct: 88,
            ..ContentPhase::default()
        }],
    );
    s.client.probe_target_kbps = Some(WEBOS_PROBE_KBPS);
    Scenario {
        name,
        seed: 0x7A_5F00 + u64::from(loss_ppm),
        duration_ms: 60_000,
        link: LinkCfg {
            capacity: vec![(0, 245_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            loss_ppm,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 168_000,
        blip_at_ms: None,
    }
}

/// Above `HEAVY_LOSS_PPM`: two windows of it are congestion.
pub(super) fn wifi_loss_heavy() -> Scenario {
    wifi_loss("wifi_loss_heavy", 25_000)
}

/// Above `SEVERE_LOSS_PPM`: one window of it is visible damage.
pub(super) fn wifi_loss_severe() -> Scenario {
    wifi_loss("wifi_loss_severe", 70_000)
}

/// A decoder whose latency rises with the rate until the cap latches, then
/// the hold and retreat bands take over. 4K165 on a fragile SoC: 6 060 µs of
/// budget, and past 60 Mbps every extra megabit costs 55 µs.
pub(super) fn decoder_knee() -> Scenario {
    let mut s = tv_session(
        20_000,
        None,
        vec![ContentPhase {
            fill_pct: 90,
            ..ContentPhase::default()
        }],
    );
    s.client.probe_target_kbps = Some(WEBOS_PROBE_KBPS);
    s.client.decode = DecodeCfg {
        base_us: 1_200,
        jitter_us: 200,
        knee_kbps: 60_000,
        us_per_mbps: 55,
    };
    Scenario {
        name: "decoder_knee",
        seed: 0x7A_6000,
        duration_ms: 240_000,
        link: LinkCfg {
            capacity: vec![(0, 245_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 168_000,
        blip_at_ms: None,
    }
}

/// A decoder that is slow at every rate: its latency never rises far enough
/// over its own floor to read as congestion, so the headroom bands are what
/// judge it — park at 80 % of the frame budget, retreat a notch at 90 %, and
/// the retreat stands only if the latency follows the rate down.
pub(super) fn decoder_headroom() -> Scenario {
    let mut sc = decoder_knee();
    sc.name = "decoder_headroom";
    sc.seed = 0x7A_6600;
    sc.sessions[0].client.decode = DecodeCfg {
        base_us: 4_900,
        jitter_us: 60,
        knee_kbps: 100_000,
        us_per_mbps: 10,
    };
    sc
}

/// A link that falls out from under the session: 245 Mbps to 2.5, with a
/// 400 ms buffer in front of it. Every window after it is starved, and the
/// frames that do arrive arrive late.
pub(super) fn starved_client() -> Scenario {
    let mut s = tv_session(20_000, None, full());
    s.client.probe_target_kbps = Some(WEBOS_PROBE_KBPS);
    Scenario {
        name: "starved_client",
        seed: 0x7A_6100,
        duration_ms: 90_000,
        link: LinkCfg {
            capacity: vec![(0, 245_000), (10_000, 2_500)],
            buffer_ms: 400,
            base_delay_ms: 20,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        // What is left after the link falls out from under the session.
        achievable_kbps: wall_respecting_kbps(2_500, 1, cap_4k165()),
        blip_at_ms: None,
    }
}

/// An encoder taking 60 ms a frame on a link with room to spare: delivery is
/// a tenth of the target with nothing lost, which is the case the starvation
/// guard exists for — `encode_us` averaged over sixteen frames a second
/// describes the interruption, not the rate.
pub(super) fn encoder_stalled() -> Scenario {
    let mut s = tv_session(20_000, None, full());
    s.client.probe_target_kbps = Some(WEBOS_PROBE_KBPS);
    s.host.loaded_encode_us = 60_000;
    s.host.loaded_from_ms = 15_000;
    s.host.encode_swing_us = 12_000;
    Scenario {
        name: "encoder_stalled",
        seed: 0x7A_6700,
        duration_ms: 90_000,
        link: LinkCfg {
            capacity: vec![(0, 245_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 168_000,
        blip_at_ms: None,
    }
}

/// A cut, then a still desktop, then motion: the first active window after
/// four repeat-only ones re-arms slow start.
pub(super) fn idle_then_motion() -> Scenario {
    let mut s = tv_session(20_000, None, full());
    s.client.probe_target_kbps = Some(WEBOS_PROBE_KBPS);
    s.host.content = vec![
        ContentPhase {
            until_ms: 20_000,
            ..ContentPhase::default()
        },
        // Long enough that a shorter proven bucket would forget the rate
        // this session had already held.
        ContentPhase {
            until_ms: 45_000,
            idle: true,
            ..ContentPhase::default()
        },
        ContentPhase::default(),
    ];
    Scenario {
        name: "idle_then_motion",
        seed: 0x7A_6200,
        duration_ms: 90_000,
        link: LinkCfg {
            capacity: vec![(0, 245_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 168_000,
        blip_at_ms: Some(15_000),
    }
}

/// A host that never answers a `SetBitrate`: after `MAX_UNACKED` unanswered
/// requests the controller goes quiet for the session.
pub(super) fn host_never_acks() -> Scenario {
    let mut s = tv_session(20_000, None, full());
    s.client.probe_target_kbps = Some(WEBOS_PROBE_KBPS);
    s.host.acks = false;
    Scenario {
        name: "host_never_acks",
        seed: 0x7A_6300,
        duration_ms: 60_000,
        link: LinkCfg {
            capacity: vec![(0, 245_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 168_000,
        blip_at_ms: None,
    }
}

/// A session whose refresh the host never named. Encode and decode
/// thresholds fall back to their absolute durations instead of frame
/// budgets, which is the only way those four constants are reached.
pub(super) fn unknown_refresh() -> Scenario {
    let mut s = tv_session(20_000, None, full());
    s.client.refresh_hz = 0;
    s.client.probe_target_kbps = Some(WEBOS_PROBE_KBPS);
    s.client.decode = DecodeCfg {
        base_us: 4_000,
        jitter_us: 500,
        knee_kbps: 40_000,
        us_per_mbps: 1_800,
    };
    Scenario {
        name: "unknown_refresh",
        seed: 0x7A_6400,
        duration_ms: 120_000,
        link: LinkCfg {
            capacity: vec![(0, 245_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 168_000,
        blip_at_ms: None,
    }
}

/// The same session with a decoder that falls off a cliff instead of a
/// slope: without a frame budget the severe tier is an absolute 45 ms, and
/// this is the only way to reach it.
pub(super) fn unknown_refresh_knee() -> Scenario {
    let mut sc = unknown_refresh();
    sc.name = "unknown_refresh_knee";
    sc.seed = 0x7A_6800;
    sc.sessions[0].client.decode.us_per_mbps = 4_000;
    sc
}

/// A host pipeline rebuild mid-session, answered with an intra-refresh wave
/// instead of an IDR.
///
/// The window the stall lands in is discarded; the client is left without a
/// reference and asks every 100 ms until a recovery point lands 600 ms later.
/// Nothing on the link moved, so those asks are the whole of the next window's
/// evidence — the case `RECOVERY_KF_BAD` and `RECOVERY_KF_SEVERE` judge,
/// outside any capacity burst. Where in its window the stall falls decides how
/// much of the wave the discarded one swallows.
fn host_rebuild(name: &'static str, seed: u64, start_kbps: u32, rebuild_at_ms: u64) -> Scenario {
    let mut s = tv_session(
        start_kbps,
        Some((0, 171_294)),
        vec![ContentPhase {
            fill_pct: 80,
            active_pct: 88,
            ..ContentPhase::default()
        }],
    );
    s.host.recovery_ms = 600;
    s.client.rebuild_at_ms = Some(rebuild_at_ms);
    Scenario {
        name,
        seed,
        duration_ms: 40_000,
        link: LinkCfg {
            capacity: vec![(0, 400_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 171_294,
        blip_at_ms: None,
    }
}

/// A stall 100 ms before the window boundary: five asks land in the judged
/// window, which is severe on one window.
pub(super) fn host_rebuild_stall() -> Scenario {
    host_rebuild("host_rebuild_stall", 0x7A_6900, 171_294, 20_150)
}

/// The same stall 400 ms earlier in its window, on a session still climbing:
/// the discarded window swallows most of the wave and two asks reach the
/// judged one. Under the severe bar, at `RECOVERY_KF_BAD` — enough to end
/// slow start, which is the whole of what it costs.
pub(super) fn host_rebuild_wave() -> Scenario {
    host_rebuild("host_rebuild_wave", 0x7A_7000, 20_000, 4_850)
}

/// The probe declined or refused: no ceiling was ever learned, so the
/// negotiated start is the whole authority.
pub(super) fn no_ramp() -> Scenario {
    Scenario {
        name: "no_ramp",
        seed: 0x7A_5C00,
        duration_ms: 60_000,
        link: LinkCfg {
            capacity: vec![(0, 400_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![SessionCfg {
            join_ms: 0,
            host: HostCfg {
                fps: 60,
                content: full(),
                ..HostCfg::default()
            },
            client: ClientCfg {
                start_kbps: 20_000,
                refresh_hz: 60,
                stream_cap_kbps: stream_ceiling_kbps(1920, 1080, 60, CODEC_H264, 8, 0),
                probe: false,
                ..ClientCfg::default()
            },
        }],
        achievable_kbps: 20_000,
        blip_at_ms: None,
    }
}

/// A bring-up faster than the ramp: video arrives with the measurement two
/// or three steps in, so the client holds a floor under the link and nothing
/// about its wall. Both of these links have one.
fn ramp_cut_short(name: &'static str, seed: u64, mut sc: Scenario, bringup_ms: u64) -> Scenario {
    sc = with_ramp(sc);
    sc.name = name;
    sc.seed = seed;
    for s in &mut sc.sessions {
        s.host.bringup_ms = s.join_ms + bringup_ms;
    }
    sc
}

/// The G5's AP, measured two steps in: a 245 Mbps wall the ramp never saw.
pub(super) fn ramp_cut_short_wifi() -> Scenario {
    ramp_cut_short(
        "ramp_cut_short_wifi",
        0x7A_7200,
        wifi_tv_probe_damage(),
        120,
    )
}

/// Klos54's tunnel, measured two steps in: a 12.5 Mbps wall, unseen.
pub(super) fn ramp_cut_short_wan() -> Scenario {
    ramp_cut_short(
        "ramp_cut_short_wan",
        0x7A_7300,
        wan_wg_12(0x7A_5500, 180_000),
        90,
    )
}

/// A host that advertises the ramp and then answers no step: the client
/// waits out the step deadline, learns nothing, and opens on the authority
/// that no measurement leaves it.
pub(super) fn ramp_unanswered() -> Scenario {
    let mut sc = wifi_tv();
    sc.name = "ramp_unanswered";
    sc.seed = 0x7A_7100;
    sc.sessions[0].host.answers_probes = false;
    sc
}

/// Ten minutes of 5120×1440@240 on a 2 GbE path: the cost model's worst
/// case, ~144 000 frames of ~490 shards each. Not in the baseline table — it
/// exists to bound the simulator's own runtime.
pub(super) fn fat_pipe_10min() -> Scenario {
    let cap = stream_ceiling_kbps(5120, 1440, 240, CODEC_HEVC, 8, 0);
    Scenario {
        name: "fat_pipe_10min",
        seed: 0x7A_5D00,
        duration_ms: 600_000,
        link: LinkCfg {
            capacity: vec![(0, 2_000_000)],
            buffer_ms: 20,
            base_delay_ms: 1,
            ..LinkCfg::default()
        },
        sessions: vec![SessionCfg {
            join_ms: 0,
            host: HostCfg {
                fps: 240,
                audio_kbps: 512,
                content: full(),
                ..HostCfg::default()
            },
            client: ClientCfg {
                start_kbps: 20_000,
                refresh_hz: 240,
                stream_cap_kbps: cap,
                audio_kbps: 512,
                ..ClientCfg::default()
            },
        }],
        achievable_kbps: 1_300_000,
        blip_at_ms: None,
    }
}

/// Every scenario the baseline pins, in table order.
///
/// Every row but `old_host` runs against a host that serves the ramp; the
/// seven calibrations run a second time against one that does not, because
/// what they replay are field sessions from before it existed.
pub(super) fn all() -> Vec<Scenario> {
    let mut table: Vec<Scenario> = vec![
        lan_10g(),
        lan_1g(),
        wifi_good(),
        wifi_tv(),
        wan_wg_12(0x7A_5500, 180_000),
        lte_variable(),
        shared_two_auto(),
        shared_newcomer(),
        shared_fixed_plus_auto(),
        gpu_saturated(),
        static_then_motion(),
        frame_driven_35fps(),
        old_host(),
        no_ramp(),
        slow_start_spent(),
        wifi_tv_probe_damage(),
        wifi_loss_heavy(),
        wifi_loss_severe(),
        decoder_knee(),
        starved_client(),
        idle_then_motion(),
        host_never_acks(),
        unknown_refresh(),
        wifi_tv_probe_stalled(),
        decoder_headroom(),
        encoder_stalled(),
        unknown_refresh_knee(),
        host_rebuild_stall(),
        host_rebuild_wave(),
        encoder_weak(),
        host_cadence_refusal(),
        ramp_unanswered(),
        ramp_cut_short_wifi(),
        ramp_cut_short_wan(),
        wan_brownout(),
    ]
    .into_iter()
    // `old_host` is the host that has none of this: it stays as it is.
    .map(|sc| {
        if sc.name == "old_host" {
            sc
        } else {
            with_ramp(sc)
        }
    })
    .collect();
    table.push(legacy(wifi_tv(), "wifi_tv_legacy"));
    table.push(legacy(wifi_good(), "wifi_good_legacy"));
    table.push(legacy(slow_start_spent(), "slow_start_spent_legacy"));
    table.push(legacy(gpu_saturated(), "gpu_saturated_legacy"));
    table.push(legacy(wan_wg_12(0x7A_5500, 180_000), "wan_wg_12_legacy"));
    table.push(legacy(
        wifi_tv_probe_damage(),
        "wifi_tv_probe_damage_legacy",
    ));
    table.push(legacy(
        wifi_tv_probe_stalled(),
        "wifi_tv_probe_stalled_legacy",
    ));
    // Four more in both shapes: a measured session runs where the rebuild's
    // asks stay under the severe bar and has no slow start left to re-arm, so
    // the old-host shape is the only witness the mutation sweep has for
    // `RECOVERY_KF_SEVERE`, `DECODE_SEVERE_US`, `DECODE_CAP_SIMILAR_DIV` and
    // `IDLE_WINDOWS_TO_REARM`.
    table.push(legacy(host_rebuild_stall(), "host_rebuild_stall_legacy"));
    table.push(legacy(decoder_knee(), "decoder_knee_legacy"));
    table.push(legacy(
        unknown_refresh_knee(),
        "unknown_refresh_knee_legacy",
    ));
    table.push(legacy(idle_then_motion(), "idle_then_motion_legacy"));
    table
}

#[cfg(test)]
mod tests {
    use super::super::{Run, WindowRec};
    use super::*;

    /// Field steps from the 09-16 trace, 19:55:46–19:56:08.
    const C1_FIELD: [u32; 12] = [
        25_247, 35_059, 41_852, 48_443, 56_240, 65_434, 74_038, 88_523, 103_536, 131_763, 166_388,
        171_294,
    ];

    /// The startup burst measures the link and nothing else: the ceiling it
    /// leaves is 0.7 × what the client received, bounded by the stream shape.
    #[test]
    fn the_startup_probe_sets_a_ceiling_from_what_it_delivered() {
        let sc = lan_1g();
        let cap = stream_ceiling_kbps(3840, 2160, 120, CODEC_HEVC, 8, 0);
        let target = crate::abr::probe::probe_target_kbps(cap);
        assert_eq!(target, 1_492_992, "twice the 4K120 stream cap");
        let want = (1_000_000u64.min(u64::from(target)) * 7 / 10).min(u64::from(cap)) as u32;
        let r = run(&sc);
        let ceiling = r.steps().into_iter().max().expect("the session climbs");
        assert!(
            ceiling * 10 >= want * 9 && ceiling <= want,
            "{ceiling} kbps against 0.7 × the 1 GbE link's {want}"
        );
    }

    /// The bring-up ramp's arithmetic, on three links whose answers differ:
    /// a 1 GbE wall above what the stream can use, a 12.5 Mbps wall three
    /// steps in, and the G5's Wi-Fi, where the measurement used to cost a
    /// 900 ms freeze.
    ///
    /// Steps, bytes and milliseconds are pinned here rather than in the
    /// baseline, which cannot see them: what the measurement costs the link
    /// is the whole point of replacing the burst.
    #[test]
    fn the_bring_up_ramp_measures_each_link_and_stops() {
        // Scenario · wall · steps · rates asked · payload KB · ms.
        for (sc, wall, steps, first, last, max_kb, max_ms) in [
            (with_ramp(lan_1g()), false, 9, 5_000, 1_066_423, 7_200, 900),
            (
                with_ramp(wan_wg_12(0x7A_5500, 180_000)),
                true,
                3,
                5_000,
                20_000,
                110,
                300,
            ),
            (
                with_ramp(wifi_tv_probe_damage()),
                true,
                7,
                5_000,
                320_000,
                2_100,
                700,
            ),
        ] {
            let name = sc.name;
            let r = run(&sc);
            let t = &r.ramps[0];
            let (at_ms, s) = t.done.expect("the ramp stops on its own");
            assert_eq!(s.wall, wall, "{name}: wall={}", s.wall);
            assert_eq!(s.steps, steps, "{name}: {:?}", t.asks);
            assert_eq!(t.asks.first().map(|a| a.1), Some(first), "{name}");
            assert_eq!(t.asks.last().map(|a| a.1), Some(last), "{name}");
            assert!(
                s.asked_bytes / 1_000 <= max_kb,
                "{name}: the ramp asked for {} KB",
                s.asked_bytes / 1_000
            );
            assert!(at_ms <= max_ms, "{name}: the ramp took {at_ms} ms");
            // L4: all of it is over before the first frame exists.
            assert!(
                at_ms < BRINGUP_MS,
                "{name}: the ramp ran {at_ms} ms into a {BRINGUP_MS} ms bring-up"
            );
        }
    }

    /// C1: the G5 reaches its measured ceiling in one climb, no cut on the
    /// way, and every step lands where the trace put it.
    #[test]
    fn c1_a_clean_start_climbs_to_the_measured_ceiling() {
        let r = run(&wifi_tv());
        let steps: Vec<u32> = r.steps().into_iter().skip(1).collect();
        assert!(r.cuts().is_empty(), "a clean start must not back off");
        assert!(
            steps.len() >= 8,
            "{} climbs, the trace made 12: {steps:?}",
            steps.len()
        );
        let at_ceiling = r.windows[0]
            .iter()
            .find(|w| w.rate_kbps >= 171_294)
            .expect("the session reaches the ceiling");
        assert!(
            (15_000..=30_000).contains(&at_ceiling.t_ms),
            "reached the ceiling at {} ms",
            at_ceiling.t_ms
        );
        for (i, (&got, &want)) in steps.iter().zip(C1_FIELD.iter()).enumerate() {
            let off = (got as i64 - want as i64) * 100 / want as i64;
            assert!(
                off.abs() <= 15,
                "step {i}: {got} kbps against the trace's {want} ({off} %)"
            );
        }
    }

    /// C2: at the ceiling with nothing else wrong, one unrecoverable frame is
    /// a blip. The rate does not move and the session holds 171 294 kbps for
    /// the rest of its run.
    ///
    /// Before: ×0.7 to 119 905 on that one window, then six additive steps —
    /// 127 400 · 135 363 · 143 824 · 152 814 · 162 365 · 171 294 — and 28 s at
    /// 30 % less picture. The 09-16 sawtooth, once every minute or two.
    #[test]
    fn c2_one_lost_frame_after_a_clean_run_is_not_a_cut() {
        let r = run(&wifi_good());
        assert!(
            r.cuts().is_empty(),
            "one lost frame on a clean link must not move the rate: {:?}",
            r.cuts().first().map(|w| (w.t_ms, w.dropped, w.recovery_kf))
        );
        let blip = r.windows[0]
            .iter()
            .find(|w| w.dropped > 0)
            .expect("the frame does die");
        assert!(
            (30_000..=31_500).contains(&blip.t_ms),
            "the injected frame died at {} ms",
            blip.t_ms
        );
        assert!(
            r.windows[0]
                .iter()
                .all(|w| w.rate_kbps == 171_294 || w.discarded),
            "the session holds the ceiling throughout"
        );
    }

    /// C3: one severe window inside the first 10 s still cuts and still ends
    /// slow start — the lost frame comes too early for a clean run to vouch
    /// for it. Eight clean utilised windows then refute the verdict, and the
    /// session doubles back instead of crawling.
    ///
    /// Before: +6 % a step from 14 000, 158 s to pass 170 000, and a `to90_s`
    /// of 187 for the scenario.
    #[test]
    fn c3_a_refuted_early_verdict_gives_slow_start_back() {
        let r = run(&slow_start_spent());
        let cut = r.cuts()[0];
        assert!(cut.t_ms <= 10_000, "the blip lands at {} ms", cut.t_ms);
        let from = r.windows[0]
            .iter()
            .find(|w| w.t_ms > cut.t_ms && w.rate_kbps == 14_000)
            .expect("20 000 × 0.7");
        let to = r.windows[0]
            .iter()
            .find(|w| w.t_ms > from.t_ms && w.rate_kbps >= 170_000)
            .expect("and it does get back");
        let took_s = (to.t_ms - from.t_ms) / 1_000;
        assert!(took_s <= 20, "14 000 → 170 000 took {took_s} s");
        // The re-arm, not a faster additive step: six seconds of clean
        // windows first, then doublings.
        let steps = r.steps();
        let rearmed = steps
            .windows(2)
            .find(|p| u64::from(p[1]) * 100 / u64::from(p[0]) >= 150)
            .expect("a doubling after the cut");
        assert_eq!(
            rearmed[0], 14_876,
            "one additive step at 14 000, then the verdict is refuted"
        );
    }

    /// A cadence refusal costs the ten seconds it lasts, not the session.
    ///
    /// Inside the refusal the controller asks for nothing above what it has;
    /// afterwards it climbs at climb-law steps, not the +12.5 % crawl above a
    /// learned cap, and lands where the same session lands with no refusal.
    #[test]
    fn a_cadence_refusal_holds_climbs_without_learning_a_cap() {
        let sc = host_cadence_refusal();
        let (from_ms, until_ms) = sc.sessions[0].host.cadence_refusal_ms;
        let r = run(&sc);
        let held: Vec<&WindowRec> = r.windows[0]
            .iter()
            .filter(|w| (from_ms..until_ms).contains(&w.t_ms))
            .collect();
        assert!(held.len() > 8, "the refusal covers {} windows", held.len());
        // One ask learns the refusal; the rest of the ten seconds is quiet.
        let asked_up = held
            .iter()
            .filter(|w| w.request_kbps.is_some_and(|k| k > w.rate_kbps))
            .count();
        assert_eq!(asked_up, 1, "kept asking a host that said it was behind");
        // Nothing was learned: the first step after the refusal is the climb
        // law's, which is never a cap lift's eighth.
        let after = r.windows[0]
            .iter()
            .filter(|w| w.t_ms >= until_ms)
            .filter_map(|w| Some((w.rate_kbps, w.request_kbps?)))
            .find(|(at, want)| want > at)
            .expect("it climbs again once the refusal lifts");
        assert!(
            u64::from(after.1) * 100 / u64::from(after.0) > 115,
            "{} → {} kbps is a cap lift, not a climb",
            after.0,
            after.1
        );
        // And it ends where an unrefused session ends.
        let mut clear = host_cadence_refusal();
        clear.sessions[0].host.cadence_refusal_ms = (u64::MAX, u64::MAX);
        let end = |r: &Run| r.windows[0].last().expect("windows").rate_kbps;
        let (refused, never) = (end(&r), end(&run(&clear)));
        assert!(
            u64::from(refused) * 100 / u64::from(never) >= 90,
            "the refusal cost the session: {refused} kbps against {never}"
        );
    }

    /// C6: the startup burst overdrives the link and video dies beside it, so
    /// the keyframe asks that outlive the discarded tail are the burst's own.
    /// They are not judged, and the session climbs on the ceiling it just
    /// measured instead of cutting.
    ///
    /// Before: four asks in the window after the tail were severe, the session
    /// went 20 000 → 14 000 with `dropped=0` and `loss_windows=0`, and never
    /// reached 90 % of the measured ceiling in the minute it ran.
    #[test]
    fn c6_the_startup_burst_no_longer_cuts_the_session_it_measures() {
        let r = run(&wifi_tv_probe_damage());
        let tail = r.windows[0]
            .iter()
            .position(|w| w.discarded)
            .expect("the burst's tail window is discarded");
        assert!(
            r.cuts().is_empty(),
            "the burst's own damage must not move the rate: {:?}",
            r.cuts().first().map(|w| (w.t_ms, w.recovery_kf))
        );
        assert_eq!(
            r.windows[0][tail + 1].recovery_kf,
            0,
            "the asks in the window after the tail belong to the burst"
        );
        let at_ceiling = r.windows[0]
            .iter()
            .find(|w| w.rate_kbps >= 151_200)
            .expect("the session reaches 90 % of the measured ceiling");
        assert!(
            at_ceiling.t_ms <= 25_000,
            "90 % of the ceiling at {} ms",
            at_ceiling.t_ms
        );
    }

    /// C7: the quieter half — two or three asks, under the severe bar but at
    /// `RECOVERY_KF_BAD`. Disowned with the rest of the burst's aftermath,
    /// they leave slow start armed and the session doubles to its ceiling.
    ///
    /// Before: that one window ended slow start with no cut and no entry in
    /// any log, and the session crawled +6 % a step — 31 134 kbps a minute in,
    /// against a ceiling of 168 000 (`host173` 09-17 10:03).
    #[test]
    fn c7_the_quiet_ask_pair_no_longer_ends_slow_start() {
        let r = run(&wifi_tv_probe_stalled());
        assert!(r.cuts().is_empty(), "nothing in this session backs off");
        let tail = r.windows[0]
            .iter()
            .position(|w| w.discarded)
            .expect("the burst's tail window is discarded");
        assert_eq!(
            r.windows[0][tail + 1].recovery_kf,
            0,
            "the asks in the window after the tail belong to the burst"
        );
        let steps = r.steps();
        assert!(steps.len() >= 8, "{} climbs", steps.len());
        for pair in steps.windows(2).take(8) {
            let pct = u64::from(pair[1]) * 100 / u64::from(pair[0]);
            assert!(
                pct >= 112,
                "{} → {} is {pct} % of the last rate — an additive step, not slow start",
                pair[0],
                pair[1]
            );
        }
        let last = r.windows[0].last().expect("the session ran");
        assert!(
            last.rate_kbps >= 151_200,
            "{} kbps a minute in, against a measured ceiling of 168 000",
            last.rate_kbps
        );
    }

    /// C4: host encode over its budget costs one notch. The encoder does not
    /// answer it, so the rate comes straight back and the down-driver stands
    /// down until a clean run re-probes it 16 windows later.
    ///
    /// Before: three ×0.7 cuts before the stand-down — 164 418 → 115 092 →
    /// 80 564 → 56 394, a third of the picture for nothing, and the same
    /// cascade on every re-arm (`.21` gamescope 4K165, 40 → 14 Mbps).
    #[test]
    fn c4_a_saturated_encoder_costs_one_notch_not_three_cuts() {
        let r = run(&gpu_saturated());
        let w = &r.windows[0];
        let disarm = w
            .iter()
            .position(|w| w.encode_disarmed)
            .expect("the encode down-driver stands down");
        let cuts_before = w[..=disarm]
            .iter()
            .filter(|w| w.cut_from_kbps.is_some())
            .count();
        assert_eq!(cuts_before, 1, "one notch, and the encoder's answer to it");
        for c in w.iter().filter(|w| w.cut_from_kbps.is_some()) {
            let from = c.cut_from_kbps.unwrap();
            let to = w
                .iter()
                .find(|x| x.t_ms > c.t_ms && x.rate_kbps < from)
                .map(|x| x.rate_kbps)
                .unwrap_or(from);
            assert_eq!(to, from - from / 8, "every step down is one notch");
        }
        // The contention arrives at 12 s; from there the rate never loses
        // more than the notch it gives back.
        let loaded = w.iter().filter(|w| w.t_ms >= 12_000);
        let (peak, floor) = loaded.fold((0, u32::MAX), |(hi, lo), w| {
            (hi.max(w.rate_kbps), lo.min(w.rate_kbps))
        });
        assert!(
            u64::from(floor) * 100 >= u64::from(peak) * 80,
            "{floor} kbps against a peak of {peak}"
        );
        let rearm = w[disarm..]
            .iter()
            .position(|w| !w.encode_disarmed)
            .expect("and it re-arms");
        assert_eq!(
            rearm, 16,
            "16 clean windows after the one that stood it down"
        );
        assert!(
            w[disarm + 1..disarm + rearm]
                .iter()
                .all(|w| w.cut_from_kbps.is_none()),
            "a stood-down driver cuts nothing"
        );
    }

    /// The other half of the same rule: an encoder whose time really is a
    /// function of the rate. Every notch is answered, so it is kept and the
    /// next one may follow — the session walks down to where the encoder
    /// keeps up instead of standing the driver down.
    #[test]
    fn a_weak_encoder_keeps_its_notches_and_walks_down() {
        let r = run(&encoder_weak());
        let w = &r.windows[0];
        assert!(
            w.iter().all(|w| !w.encode_disarmed),
            "an encoder that answers the rate never stands the driver down"
        );
        let cuts = r.cuts();
        assert!(cuts.len() >= 3, "{} notches", cuts.len());
        for c in &cuts {
            let from = c.cut_from_kbps.unwrap();
            let to = w
                .iter()
                .find(|x| x.t_ms > c.t_ms && x.rate_kbps < from)
                .map(|x| x.rate_kbps)
                .unwrap_or(from);
            assert_eq!(to, from - from / 8, "every step down is one notch");
        }
        // 3 550 µs plus 200 µs a megabit past 40 000. The second half of the
        // session averages a little over half of the 164 000 the link offers:
        // the notches took it down there, and the +6 % climb keeps testing
        // the wall between them.
        let tail = &w[w.len() / 2..];
        let mean = tail.iter().map(|w| u64::from(w.rate_kbps)).sum::<u64>() / tail.len() as u64;
        assert!(
            (60_000..110_000).contains(&mean),
            "settled around {mean} kbps"
        );
    }

    /// Ten seeded twelve-minute sessions on Klos54's path.
    fn c5_sessions() -> Vec<(u64, super::super::Run)> {
        (0..10)
            .map(|i| {
                let sc = wan_wg_12(0x5000 + i * 0x11, 720_000);
                (sc.duration_ms, run(&sc))
            })
            .collect()
    }

    /// C5: the 20 → 14 → 9.8 collapse inside the first seconds, and a wall
    /// the session keeps walking back into — his 70-session aggregate.
    #[test]
    fn c5_the_tunnel_backs_off_early_and_keeps_re_finding_its_wall() {
        let (mut wall_cuts, mut minutes) = (0u64, 0u64);
        for (i, (duration_ms, r)) in c5_sessions().into_iter().enumerate() {
            let first = r.cuts().first().map(|w| w.t_ms).unwrap_or(u64::MAX);
            assert!(first <= 10_000, "seed {i}: first cut at {first} ms");
            let at30 = r.windows[0]
                .iter()
                .find(|w| w.t_ms >= 30_000)
                .expect("30 s in");
            assert!(
                at30.rate_kbps <= 10_000,
                "seed {i}: {} kbps at 30 s",
                at30.rate_kbps
            );
            wall_cuts += r
                .cuts()
                .iter()
                .filter(|w| w.cut_from_kbps.is_some_and(|k| k >= 12_000))
                .count() as u64;
            minutes += duration_ms / 60_000;
        }
        let per_min = wall_cuts * 100 / minutes;
        assert!(
            per_min >= 30,
            "{wall_cuts} cuts from 12 Mbps or above over {minutes} min"
        );
    }

    /// C5's fourth number does not come out of one session on one link.
    ///
    /// The model spends 2–5 % of the time under 5 Mbps against the field's
    /// 22.7 %, and no link parameter inside the given ranges moves it: a
    /// cascade stops as soon as the offer drops under the capacity, which on
    /// a 10–18 Mbps path is three cuts, 20 000 → 6 860. Reaching 3 Mbps takes
    /// either a 5–8 Mbps stretch (#1131: "5–8 on a bad one") or a sibling's
    /// probe burst emptying the path (#1228 — one session floored at 2 Mbps
    /// for four minutes). Neither is in this scenario, and the second needs
    /// the probe the bring-up ramp replaces.
    #[test]
    #[ignore = "needs a worse link or a sibling probe than C5 describes"]
    fn c5_a_fifth_of_the_session_under_5_mbps() {
        let under5: u64 = c5_sessions()
            .iter()
            .map(|(_, r)| u64::from(r.metrics.under5_pct))
            .sum::<u64>()
            / 10;
        println!("under 5 Mbps: {under5} %");
        assert!(
            (10..=35).contains(&under5),
            "{under5} % of the time under 5 Mbps (the field saw 22.7 %)"
        );
    }

    /// The tunnel's delay reading survives its own cascade.
    ///
    /// Four windows of Klos54's run carried no completed AU at all, so the
    /// detector went blind exactly where the queue was deepest. A shard
    /// arrives whether or not its frame does: every window that received
    /// video now has a delay reading, ramp or no ramp.
    #[test]
    fn the_tunnel_never_goes_delay_blind_again() {
        for sc in [
            wan_wg_12(0x7A_5500, 180_000),
            with_ramp(wan_wg_12(0x7A_5500, 180_000)),
        ] {
            let audio_kbps = sc.sessions[0].client.audio_kbps;
            let r = run(&sc);
            let blind: Vec<u64> = r.windows[0]
                .iter()
                .filter(|w| !w.discarded && w.actual_kbps > audio_kbps && w.delay.is_none())
                .map(|w| w.t_ms)
                .collect();
            assert!(blind.is_empty(), "{}: blind windows at {blind:?}", sc.name);
            // And the reading is a trend, not a single point: the windows that
            // carry a cascade carry several samples each.
            let thin = r.windows[0]
                .iter()
                .filter(|w| w.delay.is_some_and(|d| d.samples < 2))
                .count();
            assert!(thin <= 1, "{}: {thin} windows with one sample", sc.name);
        }
    }

    /// `SIM_DUMP=c3 cargo test … dump -- --ignored --nocapture`: one
    /// scenario's window trail, for reading a calibration by eye.
    #[test]
    #[ignore = "a reading aid, not a check"]
    fn dump() {
        let sc = match std::env::var("SIM_DUMP").unwrap_or_default().as_str() {
            "c2" => wifi_good(),
            "c3" => slow_start_spent(),
            "c4" => gpu_saturated(),
            "c5" => wan_wg_12(0x5000, 720_000),
            "cutwifi" => ramp_cut_short_wifi(),
            "cutwan" => ramp_cut_short_wan(),
            "wan" => wan_wg_12(0x7A_5500, 180_000),
            "lte" => lte_variable(),
            "brownout" => wan_brownout(),
            "newcomer" => shared_newcomer(),
            "c6" => wifi_tv_probe_damage(),
            "knee" => decoder_knee(),
            "starved" => starved_client(),
            "unknown" => unknown_refresh(),
            "idle" => idle_then_motion(),
            "stalled" => wifi_tv_probe_stalled(),
            "rebuild" => host_rebuild_stall(),
            "wave" => host_rebuild_wave(),
            "weak" => encoder_weak(),
            _ => wifi_tv(),
        };
        // As the table has it. `SIM_LEGACY=1` reads the calibration instead.
        let sc = if std::env::var("SIM_LEGACY").is_ok() {
            sc
        } else {
            with_ramp(sc)
        };
        let r = run(&sc);
        for (i, t) in r.ramps.iter().enumerate() {
            println!("session {i} ramp {:?} asks={:?}", t.done, t.asks);
        }
        for w in &r.windows[0] {
            println!(
                "t={:6} rate={:7} actual={:7} drop={} kf={} cut={:?} disc={} dis={} \
                 delay={:?}",
                w.t_ms,
                w.rate_kbps,
                w.actual_kbps,
                w.dropped,
                w.recovery_kf,
                w.cut_from_kbps,
                w.discarded,
                w.encode_disarmed,
                w.delay,
            );
        }
        println!("metrics {:?}", r.metrics);
    }

    /// Every scenario in the plan's table runs from its fixed seed, and a run
    /// is the same run twice.
    #[test]
    fn every_scenario_runs_and_repeats_itself() {
        for sc in all() {
            let a = run(&sc);
            let b = run(&sc);
            assert_eq!(a.metrics, b.metrics, "{} is not reproducible", sc.name);
            assert!(
                !a.windows[0].is_empty(),
                "{} produced no report window",
                sc.name
            );
        }
    }
}
