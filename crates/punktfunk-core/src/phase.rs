//! Circular (directional) statistics for phase-locked capture (design/phase-locked-capture.md):
//! the client-side half of the controller's v2 error signal. Pure math, no features — shared so
//! every vsync-aware presenter (Android today, iOS next) computes the SAME statistic the host
//! controller was tuned against, and so the controller's simulation tests can generate their
//! synthetic reports through the identical code path.

/// Circular (vector-mean) statistics of latch samples against a display period: the mean latch
/// mod the period (ns) and the coherence (‰).
///
/// The mean is what a phase controller can actually steer under jitter — the MEDIAN of a
/// period-spanning distribution is immovable (shifting a uniform-mod-P distribution's mean
/// leaves its median untouched; the controller-v1 on-glass lesson, 2026-07-31). The coherence
/// (the resultant length `R` of the unit phasors, scaled to ‰) says whether ANY phase exists to
/// steer: 0 = arrivals uniformly smeared over the period (alignment is physically pointless),
/// 1000 = perfectly phase-locked.
///
/// `None` under 8 samples or a non-positive period — too little evidence to report a phase.
pub fn circular_latch(samples_us: &[u64], period_ns: i64) -> Option<(u64, u16)> {
    if samples_us.len() < 8 || period_ns <= 0 {
        return None;
    }
    let period_us = period_ns as f64 / 1000.0;
    let (mut x, mut y) = (0.0f64, 0.0f64);
    for &s in samples_us {
        let theta = (s as f64 % period_us) / period_us * std::f64::consts::TAU;
        x += theta.cos();
        y += theta.sin();
    }
    let n = samples_us.len() as f64;
    let r = (x * x + y * y).sqrt() / n;
    let mean_theta = y.atan2(x).rem_euclid(std::f64::consts::TAU);
    let mean_ns = (mean_theta / std::f64::consts::TAU * period_ns as f64) as u64;
    Some((mean_ns, (r * 1000.0) as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: i64 = 8_333_333; // 120 Hz in ns
    const P_US: u64 = 8_333; // …and in µs, the sample unit

    #[test]
    fn identical_samples_are_fully_coherent() {
        let (mean, coh) = circular_latch(&[4_000; 16], P).unwrap();
        assert!(coh >= 995, "identical phases must read ~1000‰, got {coh}");
        assert!(
            (mean as i64 - 4_000_000).abs() < 20_000,
            "mean {mean} ≉ 4.0 ms"
        );
    }

    #[test]
    fn uniform_grid_over_the_period_is_incoherent() {
        // 16 samples evenly spanning one period — the resultant vector cancels.
        let samples: Vec<u64> = (0..16).map(|i| i * P_US / 16).collect();
        let (_, coh) = circular_latch(&samples, P).unwrap();
        assert!(coh < 100, "a uniform phase smear must read ~0‰, got {coh}");
    }

    #[test]
    fn cluster_straddling_the_wrap_averages_at_the_boundary() {
        // Half the samples just below the period boundary, half just above 0: an ARITHMETIC
        // mean would report ~P/2 (maximally wrong); the circular mean must sit at the boundary.
        let samples = [
            P_US - 200,
            P_US - 100,
            P_US - 50,
            P_US - 150,
            100,
            50,
            150,
            200,
        ];
        let (mean, coh) = circular_latch(&samples, P).unwrap();
        let dist_to_boundary = (mean as i64).min((P - mean as i64).abs());
        assert!(
            dist_to_boundary < 500_000,
            "circular mean {mean} must hug the wrap boundary"
        );
        assert!(
            coh > 900,
            "a tight straddling cluster is still coherent, got {coh}"
        );
    }

    #[test]
    fn too_few_samples_report_nothing() {
        assert!(circular_latch(&[1_000; 7], P).is_none());
        assert!(circular_latch(&[1_000; 16], 0).is_none());
    }
}
