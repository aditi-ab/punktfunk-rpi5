//! Thin `tracing` layer feeding `pf_client_core::logring` — the source for the console's
//! "Send logs to host" action. Captures at DEBUG+ regardless of `RUST_LOG` (its own filter is
//! applied at install), mirroring the host's `log_capture::RingLayer`: the whole point is that
//! a field report carries the diagnostics nobody thought to enable beforehand.

use std::fmt::Write as _;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;

pub(crate) struct RingLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        struct V(String);
        impl Visit for V {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    // The message leads; fields follow. Events put it first anyway, so
                    // this is belt-and-braces against odd macro orderings.
                    let rest = std::mem::take(&mut self.0);
                    let _ = write!(self.0, "{value:?}");
                    self.0.push_str(&rest);
                } else {
                    let _ = write!(self.0, " {}={:?}", field.name(), value);
                }
            }
        }
        let mut v = V(String::new());
        event.record(&mut v);
        let meta = event.metadata();
        pf_client_core::logring::note(format!(
            "{} {:5} {} {}",
            wallclock(),
            meta.level().as_str(),
            meta.target(),
            v.0
        ));
    }
}

/// `2026-08-15T12:03:47.123Z` from the system clock — wall time, so a bundle correlates with
/// the host log it lands next to. No chrono dep; same civil-date derivation the host uses.
fn wallclock() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z",
        ms % 1000
    )
}
