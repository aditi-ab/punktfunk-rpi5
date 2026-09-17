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
use crate::abr::stream_ceiling_kbps;
use crate::quic::CODEC_HEVC;

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
            Some((1_000, 171_294)),
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
            None,
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
            Some((1_000, 171_294)),
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
    Scenario {
        name: "gpu_saturated",
        seed: 0x7A_5400,
        duration_ms: 80_000,
        link: LinkCfg {
            capacity: vec![(0, 400_000)],
            buffer_ms: 60,
            base_delay_ms: 3,
            ..LinkCfg::default()
        },
        sessions: vec![s],
        achievable_kbps: 40_000,
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
        achievable_kbps: 12_000,
        blip_at_ms: None,
    }
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
                ceiling_at: Some((1_000, 1_300_000)),
                ..ClientCfg::default()
            },
        }],
        achievable_kbps: 1_300_000,
        blip_at_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Field steps from the 09-16 trace, 19:55:46–19:56:08.
    const C1_FIELD: [u32; 12] = [
        25_247, 35_059, 41_852, 48_443, 56_240, 65_434, 74_038, 88_523, 103_536, 131_763, 166_388,
        171_294,
    ];

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

    /// C2: one unrecoverable frame is one cut to 0.7×, and the climb back is
    /// six additive steps — the 09-16 sawtooth.
    #[test]
    fn c2_one_lost_frame_costs_a_cut_and_half_a_minute() {
        let r = run(&wifi_good());
        let cuts = r.cuts();
        assert_eq!(cuts.len(), 1, "one lost frame, one cut");
        let cut = cuts[0];
        assert_eq!(cut.cut_from_kbps, Some(171_294));
        let after = r.windows[0]
            .iter()
            .find(|w| w.t_ms > cut.t_ms && w.rate_kbps < 171_294)
            .expect("the cut lands");
        assert_eq!(after.rate_kbps, 119_905, "0.7 × the ceiling");
        let back = r.windows[0]
            .iter()
            .find(|w| w.t_ms > cut.t_ms && w.rate_kbps >= 171_294)
            .expect("and it climbs back");
        let took = back.t_ms - cut.t_ms;
        assert!(
            (24_000..=32_000).contains(&took),
            "back at the ceiling after {took} ms"
        );
        let steps = r.windows[0]
            .iter()
            .filter(|w| w.t_ms > cut.t_ms && w.t_ms <= back.t_ms)
            .fold((0u32, 119_905u32), |(n, last), w| {
                if w.rate_kbps > last {
                    (n + 1, w.rate_kbps)
                } else {
                    (n, last)
                }
            })
            .0;
        assert_eq!(
            steps, 6,
            "127 400 · 135 363 · 143 824 · 152 814 · 162 365 · 171 294"
        );
    }

    /// C3: one severe window inside the first 10 s ends slow start for the
    /// session, and the crawl back costs minutes.
    #[test]
    fn c3_one_early_verdict_spends_slow_start_for_good() {
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
        assert!(took_s >= 150, "14 000 → 170 000 took {took_s} s");
    }

    /// C4: host encode over its budget cuts twice more, then the down-driver
    /// stands down and nothing cuts until it re-arms 16 windows later.
    #[test]
    fn c4_a_saturated_encoder_cuts_twice_then_stands_down() {
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
        assert_eq!(
            cuts_before, 3,
            "the first verdict plus the two no-op backoffs"
        );
        for c in w[..=disarm].iter().filter(|w| w.cut_from_kbps.is_some()) {
            let from = c.cut_from_kbps.unwrap();
            let to = w
                .iter()
                .find(|x| x.t_ms > c.t_ms && x.rate_kbps < from)
                .map(|x| x.rate_kbps)
                .unwrap_or(from);
            assert_eq!(to, (from as u64 * 7 / 10) as u32, "every cut is ×0.7");
        }
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
        assert!(
            (10..=35).contains(&under5),
            "{under5} % of the time under 5 Mbps (the field saw 22.7 %)"
        );
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
            _ => wifi_tv(),
        };
        let r = run(&sc);
        for w in &r.windows[0] {
            println!(
                "t={:6} rate={:7} actual={:7} drop={} cut={:?} disc={} dis={}",
                w.t_ms,
                w.rate_kbps,
                w.actual_kbps,
                w.dropped,
                w.cut_from_kbps,
                w.discarded,
                w.encode_disarmed
            );
        }
        println!("metrics {:?}", r.metrics);
    }
}
