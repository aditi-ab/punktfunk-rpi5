//! `plan_au` time against AU size, for the NAL scan. A timing, not a check:
//! `cargo test -p pf-bitstream --release --features test-vectors --test scan_timing -- --ignored --nocapture`

use pf_bitstream::h264::H264Planner;
use pf_bitstream::h265::H265Planner;
use pf_bitstream::testing;
use std::time::Instant;

/// `au` with its last NAL's payload grown to `bytes` of 0xFF: more to scan, nothing new to parse.
fn grown(au: &[u8], bytes: usize) -> Vec<u8> {
    let mut v = au.to_vec();
    v.resize(bytes.max(v.len()), 0xFF);
    v
}

fn mean_us(mut f: impl FnMut()) -> f64 {
    const N: u32 = 200;
    f();
    let t0 = Instant::now();
    for _ in 0..N {
        f();
    }
    t0.elapsed().as_secs_f64() * 1e6 / f64::from(N)
}

#[test]
#[ignore = "prints a timing"]
fn plan_au_time_per_au_size() {
    for bytes in [100_000usize, 1_000_000] {
        let au = grown(testing::split_h264_aus(testing::H264_25FPS)[0], bytes);
        let us = mean_us(|| {
            H264Planner::new().plan_au(&au).expect("plans");
        });
        println!("h264 plan_au {bytes} B: {us:.1} us");
        let au = grown(testing::split_h265_aus(testing::H265_25FPS)[0], bytes);
        let us = mean_us(|| {
            H265Planner::new().plan_au(&au).expect("plans");
        });
        println!("h265 plan_au {bytes} B: {us:.1} us");
    }
}
