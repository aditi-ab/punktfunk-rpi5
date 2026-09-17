//! The checked-in baseline: what today's controller does on every scenario.
//!
//! The simulator is integer-only and seeded, so the test is equality, not a
//! tolerance. A change that moves a cell re-blesses `baseline.tsv` in the same
//! diff (`ABR_SIM_BLESS=1 cargo test …`) and the reviewer reads which rows
//! moved and why.

use super::{run, scenarios, Metrics};

const BASELINE: &str = include_str!("baseline.tsv");

const HEADER: &str = "scenario\tunder5_pct\tto90_s\tcuts_10min\tlost_10min\t\
                      queue_p95_ms\tover_cap_kb_10s\tblip_recover_s\tfairness_x1000\t\
                      decisions_fnv1a";

fn row(name: &str, m: &Metrics) -> String {
    format!(
        "{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:08x}",
        m.under5_pct,
        m.to90_s,
        m.cuts_per_10min,
        m.lost_per_10min,
        m.queue_p95_ms,
        m.over_cap_kb_10s,
        m.blip_recover_s,
        m.fairness_x1000,
        m.decisions_fnv1a
    )
}

fn table() -> String {
    let mut out = String::from(HEADER);
    for sc in scenarios::all() {
        out.push('\n');
        out.push_str(&row(sc.name, &run(&sc).metrics));
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every cell, compared for equality. `ABR_SIM_BLESS=1` rewrites the file.
    #[test]
    fn the_baseline_is_what_todays_controller_does() {
        let got = table();
        if std::env::var("ABR_SIM_BLESS").is_ok_and(|v| v != "0") {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/abr/sim/baseline.tsv");
            std::fs::write(path, &got).expect("rewrite the baseline");
            return;
        }
        for (want, got) in BASELINE.lines().zip(got.lines()) {
            assert_eq!(want, got, "baseline row moved — re-bless it deliberately");
        }
        assert_eq!(
            BASELINE.lines().count(),
            got.lines().count(),
            "the scenario table and the baseline have different rows"
        );
    }
}
