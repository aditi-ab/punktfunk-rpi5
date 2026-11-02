//! Media packets through the client's reassembler: the header firewall, block state, parity
//! recovery and the in-flight budget, on a burst a host or the path could produce. `core_wire`
//! reads the header alone. Each packet is length-prefixed, so one input is a whole burst into
//! one stateful reassembler, on either erasure scheme.
#![no_main]

use libfuzzer_sys::fuzz_target;
use punktfunk_core::config::{Config, FecScheme, Role};
use punktfunk_core::fec::coder_for;
use punktfunk_core::packet::{Reassembler, ReassemblerLimits};
use punktfunk_core::stats::StatsCounters;

fuzz_target!(|data: &[u8]| {
    let Some((&arm, mut rest)) = data.split_first() else {
        return;
    };
    let mut cfg = Config::defaults(Role::Client);
    cfg.fec.scheme = if arm & 1 == 0 {
        FecScheme::Gf8
    } else {
        FecScheme::Gf16
    };
    let coder = coder_for(cfg.fec.scheme);
    let stats = StatsCounters::default();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg), 0);
    r.set_deliver_parts(arm & 2 != 0);
    r.set_deliver_partial(arm & 4 != 0);
    while let [l0, l1, tail @ ..] = rest {
        let n = usize::from(u16::from_le_bytes([*l0, *l1])).min(tail.len());
        let (pkt, next) = tail.split_at(n);
        let _ = r.push(pkt, coder.as_ref(), &stats);
        let _ = r.take_partial();
        rest = next;
    }
    let _ = r.take_shard_delays().count();
    let _ = r.take_short_tails().count();
    let _ = r.take_loss_positions();
});
