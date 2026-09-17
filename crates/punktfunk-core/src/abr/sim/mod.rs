//! Link simulator: today's controller in a closed loop with models of the
//! link, the host and the client.
//!
//! Integer arithmetic (kbps, bytes, µs) and an inline splitmix64 seeded per
//! scenario, so a run is bit-identical on macOS arm64 and Linux x86_64 and
//! survives a `rand` bump. Time is a 1 ms tick and an `Instant` is
//! `base + Duration`. [`scenarios`] holds the scenario table and the field
//! calibration; [`baseline`] pins what today's controller does on each one.
//!
//! Nothing here changes production behaviour: the controller is the fixed
//! point, and a behaviour that will not reproduce is a finding about the
//! model, not licence to tune the controller.

mod client;
mod host;
mod link;

/// splitmix64. One line of state, no dependency, identical everywhere.
#[derive(Clone, Debug)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x2545F491_4F6CDD1D) ^ 0x9E3779B9_7F4A7C15)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B9_7F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D_1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB_133111EB);
        z ^ (z >> 31)
    }

    /// Uniform over `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    fn chance_ppm(&mut self, ppm: u32) -> bool {
        self.below(1_000_000) < u64::from(ppm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generator is the same stream everywhere, so a baseline row is a
    /// fact and not a platform's opinion.
    #[test]
    fn the_generator_is_pinned() {
        let mut r = Rng::new(7);
        assert_eq!(
            [r.next_u64(), r.next_u64(), r.next_u64()],
            [
                16_557_362_563_216_862_149,
                430_200_180_043_962_517,
                5_998_290_083_107_941_422
            ]
        );
        let mut r = Rng::new(7);
        let hits = (0..10_000).filter(|_| r.chance_ppm(250_000)).count();
        assert_eq!(hits, 2_481, "a quarter of the draws, to the draw");
    }
}
