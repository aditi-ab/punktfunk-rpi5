//! Thin `tracing` layer feeding `pf_client_core::logring` — the source for the console's
//! "Send logs to host" action. Captures at DEBUG+ regardless of `RUST_LOG` (its own filter is
//! applied at install), mirroring the host's `log_capture::RingLayer`: the whole point is that
//! a field report carries the diagnostics nobody thought to enable beforehand.
//!
//! …which is exactly why it also has to keep OUT the chatter that would evict them. The ring
//! holds 4096 lines. The vendored H.265 parser (`cros_codecs`, behind `pf-bitstream`) DEBUG-logs
//! its DPB bookkeeping — "Retaining pic POC", "Stored picture", "Set reference", "Bumping POC",
//! one `find_short_term_ref_by_poc` per reference — a dozen lines PER FRAME, so at 120 fps the
//! ring turns over in about three seconds. The 2026-08-17 field bundle from a Steam Deck read
//! `… 2037456 older lines evicted from the ring …` followed by 3.5 s of DPB chatter: the whole
//! 27-minute session, including the 10 s `audio playback buffer_ms= underruns=` line three
//! investigation rounds had been waiting for, was gone. A field ring that a healthy decoder can
//! flush is worse than no ring, because it looks like diagnostics and carries none.

use std::fmt::Write as _;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;

/// Targets whose DEBUG/TRACE output is steady-state per-frame chatter, not diagnostics. The ring
/// keeps their INFO-and-up. Prefix-matched on module-path boundaries, so `cros_codecs::codec::…`
/// is gated and a hypothetical `cros_codecs_probe` is not. Same shape as the host's
/// `log_capture::NOISY_DEBUG_TARGETS`.
const NOISY_DEBUG_TARGETS: &[&str] = &["cros_codecs"];

fn is_noisy_debug(target: &str) -> bool {
    NOISY_DEBUG_TARGETS.iter().any(|t| {
        target
            .strip_prefix(t)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

pub(crate) struct RingLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        // Events from `log`-crate dependencies (the vendored decoder among them) arrive through
        // the tracing-log bridge under the shim target "log", with the record's real module path
        // tucked into `log.target=`. Normalize back to the real metadata so the noise gate below
        // and the target column both see `cros_codecs::…` — under the shim target every bridged
        // event is indistinguishable from every other, and the field bundle's target column read
        // `log` for two million lines.
        use tracing_log::NormalizeEvent;
        let normalized = event.normalized_metadata();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());
        if *meta.level() > tracing::Level::INFO && is_noisy_debug(meta.target()) {
            return;
        }
        struct V(String);
        impl Visit for V {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    // The message leads; fields follow. Events put it first anyway, so
                    // this is belt-and-braces against odd macro orderings.
                    let rest = std::mem::take(&mut self.0);
                    let _ = write!(self.0, "{value:?}");
                    self.0.push_str(&rest);
                } else if !field.name().starts_with("log.") {
                    // `log.target`/`log.module_path`/`log.file`/`log.line` are the bridge's own
                    // bookkeeping — already surfaced through the normalized target above, and
                    // 150 bytes of repeated path per line otherwise.
                    let _ = write!(self.0, " {}={:?}", field.name(), value);
                }
            }
        }
        let mut v = V(String::new());
        event.record(&mut v);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate is a prefix match on module-path boundaries, nothing looser.
    #[test]
    fn noisy_gate_matches_the_crate_and_its_modules_only() {
        assert!(is_noisy_debug("cros_codecs"));
        assert!(is_noisy_debug("cros_codecs::codec::h265::dpb"));
        assert!(!is_noisy_debug("cros_codecs_probe"));
        assert!(!is_noisy_debug("pf_bitstream::h265"));
        assert!(!is_noisy_debug("pf_client_core::audio"));
    }

    /// End to end through the bridge: a `log::debug!` from the vendored decoder's module path
    /// must NOT reach the ring, its `warn!` must (under its real target, without the bridge's
    /// bookkeeping fields), and a DEBUG event from our own audio module — the very line the gate
    /// exists to protect — must land.
    ///
    /// The ring is process-global, so the assertions look for lines this test wrote (unique
    /// markers) rather than at the ring's size, and the subscriber is installed only for the
    /// duration of the test.
    #[test]
    fn bridged_decoder_debug_is_dropped_and_the_audio_line_survives() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Layer;
        let sub = tracing_subscriber::registry()
            .with(RingLayer.with_filter(tracing_subscriber::filter::LevelFilter::DEBUG));
        // The bridge may already be installed by another test in this binary; either way the
        // `log` max level has to admit DEBUG for the planted records to be dispatched at all.
        let _ = tracing_log::LogTracer::builder()
            .with_max_level(log::LevelFilter::Debug)
            .init();
        log::set_max_level(log::LevelFilter::Debug);
        let _guard = tracing::subscriber::set_default(sub);

        let marker = format!("ringgate-{}", std::process::id());
        log::debug!(target: "cros_codecs::codec::h265::dpb", "Retaining pic POC {marker}-dpb: true");
        log::warn!(target: "cros_codecs::codec::h265::parser", "{marker}-parser-warn");
        tracing::debug!(target: "pf_client_core::audio", buffer_ms = 15u32, "audio playback {marker}-audio");

        let text = pf_client_core::logring::render("test");
        assert!(
            !text.contains(&format!("{marker}-dpb")),
            "decoder DPB DEBUG chatter must not reach the ring"
        );
        let warn_line = text
            .lines()
            .find(|l| l.contains(&format!("{marker}-parser-warn")))
            .expect("decoder WARN must be kept");
        assert!(
            warn_line.contains("cros_codecs::codec::h265::parser"),
            "bridged events must carry their real target, not the `log` shim: {warn_line}"
        );
        assert!(
            !warn_line.contains("log.target="),
            "bridge bookkeeping fields must be dropped: {warn_line}"
        );
        let audio_line = text
            .lines()
            .find(|l| l.contains(&format!("{marker}-audio")))
            .expect("our own DEBUG audio line must survive");
        assert!(audio_line.contains("pf_client_core::audio"));
        assert!(audio_line.contains("buffer_ms=15"));
    }
}
