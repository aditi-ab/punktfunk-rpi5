//! `loss-harness` — sweep packet loss against the FEC and report recovery (plan §10).
//!
//! Drives access units through the in-process loopback at increasing loss rates, for
//! both FEC schemes, and prints how many frames survive. A pure-software stand-in for
//! `tc netem` that needs no network and runs anywhere `punktfunk_core` builds. The real M3
//! harness adds `tc netem` jitter/reorder on the UDP path.

use punktfunk_core::config::{Config, FecConfig, FecScheme, ProtocolPhase, Role};
use punktfunk_core::error::PunktfunkError;
use punktfunk_core::session::Session;
use punktfunk_core::transport::loopback_pair;

fn config(role: Role, scheme: FecScheme, drop_period: u32) -> Config {
    Config {
        role,
        phase: match scheme {
            FecScheme::Gf8 => ProtocolPhase::P1GameStream,
            FecScheme::Gf16 => ProtocolPhase::P2Punktfunk,
        },
        fec: FecConfig {
            scheme,
            fec_percent: 25,
            max_data_per_block: 64,
        },
        shard_payload: 1024,
        max_frame_bytes: 8 * 1024 * 1024,
        encrypt: false,
        key: [0u8; 16],
        salt: [0u8; 4],
        loopback_drop_period: drop_period,
    }
}

/// Returns (frames_completed, frames_attempted) for a loss setting.
fn run(scheme: FecScheme, drop_period: u32, frames: usize, frame_len: usize) -> (usize, usize) {
    let (h, c) = loopback_pair(drop_period, 0);
    let mut host = Session::new(config(Role::Host, scheme, drop_period), Box::new(h)).unwrap();
    let mut client = Session::new(config(Role::Client, scheme, drop_period), Box::new(c)).unwrap();

    let mut completed = 0;
    for f in 0..frames {
        let frame: Vec<u8> = (0..frame_len).map(|b| (b ^ f) as u8).collect();
        host.submit_frame(&frame, f as u64, 0).unwrap();
        match client.poll_frame() {
            Ok(got) => {
                if got.data == frame {
                    completed += 1;
                }
            }
            Err(PunktfunkError::NoFrame) => {} // unrecoverable at this loss rate
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    (completed, frames)
}

fn main() {
    let frames = 50;
    let frame_len = 100_000; // ~98 shards across 2 FEC blocks
    let periods = [0u32, 32, 16, 8, 6, 4, 3, 2];

    println!("punktfunk loss-harness — 25% FEC, {frames} frames of {frame_len} bytes");
    println!("(GF8 = P1/GameStream-compat, GF16 = P2/wall-breaker)\n");
    println!(
        "{:>10}  {:>9}  {:>14}  {:>14}",
        "drop 1/N", "~loss %", "GF8 recovered", "GF16 recovered"
    );
    println!("{}", "-".repeat(56));
    for &p in &periods {
        let loss = if p == 0 { 0.0 } else { 100.0 / p as f64 };
        let (g8, n) = run(FecScheme::Gf8, p, frames, frame_len);
        let (g16, _) = run(FecScheme::Gf16, p, frames, frame_len);
        let label = if p == 0 {
            "none".to_string()
        } else {
            format!("1/{p}")
        };
        println!("{label:>10}  {loss:>8.1}%  {:>11}/{n}  {:>11}/{n}", g8, g16);
    }
    println!("\nNote: recovery drops off once per-block loss exceeds the 25% recovery budget.");
}
