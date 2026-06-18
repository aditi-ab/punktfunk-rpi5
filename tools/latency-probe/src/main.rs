//! `latency-probe` — glass-to-glass latency measurement (plan §10).
//!
//! Renders a timestamp/QR on the host, reads it back on the client capture (or a
//! photodiode for true photons), and tracks p50/p99. Build this before optimizing
//! anything, so regressions are quantifiable.
//!
//! Status: scaffold.

fn main() {
    println!(
        "latency-probe: scaffold (render timestamp/QR on host, read back on client; track p50/p99)"
    );
}
