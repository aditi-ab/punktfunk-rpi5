//! `clients/shared/cadence-vectors.json`: scripted runs of [`CadenceClock`] with what each step
//! answered. The Apple client keeps a Swift port of the loop (`CadenceClock.swift`) and replays
//! this file against it; Android runs this crate's loop itself.
//!
//! The scripts live here; the file is their output. After a deliberate change to the loop, rewrite
//! it with `cargo test -p punktfunk-core --lib write_cadence_vectors -- --ignored` and review the
//! diff.

use super::tests::{pts_at, Lcg, DELAY, DOMAIN, P, PTS0};
use super::*;
use serde_json::{json, Value};

const FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../clients/shared/cadence-vectors.json"
);

/// When a frame stamped `pts` is presentable: the domain, the delay, and `extra`.
fn ready_at(pts: u64, extra: i64) -> i64 {
    pts as i64 + DOMAIN + DELAY + extra
}

enum Op {
    Due { pts: u64, ready: i64 },
    OffCadence { ready: i64 },
    Reset,
}

/// One scripted run: every op, what it answered, and the loop's health after the last.
fn case(name: &str, tuning_name: &str, tuning: CadenceTuning, ops: &[Op]) -> Value {
    let mut clock = CadenceClock::new(tuning);
    let steps: Vec<Value> = ops
        .iter()
        .map(|op| match *op {
            Op::Due { pts, ready } => {
                json!({ "due": [pts, ready, P], "expect": clock.due_ns(pts, ready, P) })
            }
            Op::OffCadence { ready } => {
                json!({ "off_cadence": [ready, P], "expect": clock.note_off_cadence(ready, P) })
            }
            Op::Reset => {
                clock.reset();
                json!({ "reset": true })
            }
        })
        .collect();
    let h = clock.health();
    json!({
        "name": name,
        "tuning": tuning_name,
        "steps": steps,
        "health": {
            "frames": h.frames,
            "late": h.late,
            "reanchors": h.reanchors,
            "offset_ns": h.offset_ns,
            "skew_ns": h.skew_ns,
            "jitter_ns": h.jitter_ns,
            "cushion_ns": h.cushion_ns,
        },
    })
}

/// `n` frames from `from` on the source's own grid, each with transport noise.
fn frames(from: i64, n: i64, extra: i64, spread_ns: i64, rng: &mut Lcg) -> Vec<Op> {
    (from..from + n)
        .map(|k| {
            let pts = pts_at(k);
            Op::Due {
                pts,
                ready: ready_at(pts, extra + rng.noise(spread_ns)),
            }
        })
        .collect()
}

fn cases() -> Vec<Value> {
    let snapping = CadenceTuning::snapping();
    let mut out = Vec::new();

    out.push(case(
        "steady_link_with_one_millisecond_of_jitter",
        "snapping",
        snapping,
        &frames(0, 120, 0, 1_000_000, &mut Lcg(7)),
    ));

    // 400 ns per frame is about 48 ppm, an ordinary crystal pair: the rate term follows it.
    let ramp: Vec<Op> = (0..160i64)
        .map(|k| {
            let pts = pts_at(k);
            Op::Due {
                pts,
                ready: ready_at(pts, k * 400),
            }
        })
        .collect();
    out.push(case("clock_ramp", "snapping", snapping, &ramp));

    // A stall, a pause, time running backwards, an off-cadence frame, a reset and a late frame.
    let mut rng = Lcg(7);
    let mut ops = frames(0, 80, 0, 200_000, &mut rng);
    let k = 80;
    ops.push(Op::Due {
        pts: pts_at(k),
        ready: ready_at(pts_at(k), 500_000_000),
    });
    ops.extend(frames(k + 1, 20, 0, 200_000, &mut rng));
    let far = pts_at(100) + 2_000_000_000;
    ops.push(Op::Due {
        pts: far,
        ready: ready_at(far, 4_000_000),
    });
    for j in 1..=10i64 {
        let pts = far + (j * P) as u64;
        ops.push(Op::Due {
            pts,
            ready: ready_at(pts, rng.noise(200_000)),
        });
    }
    let back = pts_at(20);
    ops.push(Op::Due {
        pts: back,
        ready: ready_at(back, 0),
    });
    ops.extend(frames(21, 5, 0, 200_000, &mut rng));
    ops.push(Op::OffCadence { ready: 1_000_000 });
    ops.push(Op::Reset);
    ops.extend(frames(200, 10, 0, 200_000, &mut rng));
    let late = pts_at(210);
    ops.push(Op::Due {
        pts: late,
        ready: ready_at(late, 30_000_000),
    });
    out.push(case(
        "stall_gap_regression_off_cadence_reset_and_a_late_frame",
        "snapping",
        snapping,
        &ops,
    ));

    // Jitter wider than a frame: the cushion stops at one interval.
    out.push(case(
        "jitter_wider_than_a_frame_under_free_running",
        "free_running",
        CadenceTuning::free_running(),
        &frames(0, 140, 0, P * 3 / 2, &mut Lcg(17)),
    ));

    // A source that alternates half and one-and-a-half intervals is reproduced, not evened out.
    let mut rng = Lcg(13);
    let mut pts = PTS0;
    let irregular: Vec<Op> = (0..140)
        .map(|k| {
            pts = (pts as i64 + if k % 2 == 0 { P / 2 } else { P * 3 / 2 }) as u64;
            Op::Due {
                pts,
                ready: ready_at(pts, rng.noise(500_000)),
            }
        })
        .collect();
    out.push(case(
        "irregular_source_cadence",
        "snapping",
        snapping,
        &irregular,
    ));
    out
}

fn vectors() -> Value {
    json!({
        "$comment": "Written by punktfunk-core's phase::vectors (see its module doc); the Apple client's CadenceClockTests replays it. Each case runs one CadenceClock under the named tuning (CadenceTuning::snapping or free_running). A `due` step is [src_pts_ns, ready_ns, frame_interval_ns] and expect is the due time it returns; `off_cadence` is [ready_ns, frame_interval_ns] and `reset` forces a re-anchor. `health` is the loop's counters and estimate after the last step. Everything is integer nanoseconds and compared exactly.",
        "version": 1,
        "cases": cases(),
    })
}

/// The file's text: a step per line, rather than a number per line.
fn layout(v: &Value) -> String {
    let line = |v: &Value| serde_json::to_string(v).expect("serialize");
    let cases = v["cases"].as_array().expect("cases");
    let mut out = format!(
        "{{\n  \"$comment\": {},\n  \"version\": {},\n  \"cases\": [\n",
        line(&v["$comment"]),
        line(&v["version"])
    );
    for (i, case) in cases.iter().enumerate() {
        let steps = case["steps"].as_array().expect("steps");
        out.push_str(&format!(
            "    {{\n      \"name\": {},\n      \"tuning\": {},\n      \"steps\": [\n",
            line(&case["name"]),
            line(&case["tuning"])
        ));
        for (j, step) in steps.iter().enumerate() {
            let comma = if j + 1 < steps.len() { "," } else { "" };
            out.push_str(&format!("        {}{comma}\n", line(step)));
        }
        let comma = if i + 1 < cases.len() { "," } else { "" };
        out.push_str(&format!(
            "      ],\n      \"health\": {}\n    }}{comma}\n",
            line(&case["health"])
        ));
    }
    out.push_str("  ]\n}\n");
    out
}

#[test]
fn the_checked_in_vectors_are_what_the_clock_answers() {
    let raw = include_str!("../../../../../clients/shared/cadence-vectors.json");
    let file: Value = serde_json::from_str(raw).expect("vector file parses");
    assert!(
        file == vectors(),
        "clients/shared/cadence-vectors.json is stale: rewrite it (module doc) and review the diff"
    );
}

#[test]
#[ignore = "rewrites clients/shared/cadence-vectors.json"]
fn write_cadence_vectors() {
    std::fs::write(FILE, layout(&vectors())).expect("write the vector file");
}
