//! The client's own recent-log ring + the "send logs to host" uploader.
//!
//! Why: on locked-down platforms (a Steam Deck in Gaming Mode, tvOS, webOS) the user cannot get
//! the client's log off the device, so field reports arrive host-log-only and the client half of
//! any stutter/latency story is invisible. The cure is inverted collection: the client keeps its
//! newest few thousand log lines here, and an explicit user action posts them to the PAIRED host
//! (`POST /api/v1/client-logs`, authenticated by the same mTLS identity the stream uses), where
//! the web console lists them next to the host's own logs.
//!
//! The ring itself is dependency-free (std only — Android renders it too). The [`RingLayer`]
//! that feeds it from `tracing` lives right beside it, desktop-gated: it started as
//! `punktfunk-session`'s private `ring_layer`, and the moment the GTK and WinUI shells wanted
//! "Send logs to host" too, a copy per bin was exactly the drift this crate exists to prevent.
//! Bounded by lines AND bytes so a log-storm can't grow memory; the byte budget stays under the
//! host's 1 MiB upload cap so a full ring always uploads whole.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

/// Newest lines kept (matches the host's own ring depth).
pub const MAX_LINES: usize = 4096;

/// Byte budget for the ring — under the host's 1 MiB bundle cap with headroom for the header.
pub const MAX_BYTES: usize = 768 * 1024;

struct Ring {
    lines: VecDeque<String>,
    bytes: usize,
    dropped: u64,
}

static RING: LazyLock<Mutex<Ring>> = LazyLock::new(|| {
    Mutex::new(Ring {
        lines: VecDeque::new(),
        bytes: 0,
        dropped: 0,
    })
});

/// Append one formatted log line (no trailing newline). Oversized lines are truncated to keep a
/// single event from evicting the whole ring.
pub fn note(mut line: String) {
    if line.len() > 2048 {
        line.truncate(2048);
        line.push('…');
    }
    let mut r = RING.lock().unwrap_or_else(|e| e.into_inner());
    r.bytes += line.len();
    r.lines.push_back(line);
    while r.lines.len() > MAX_LINES || r.bytes > MAX_BYTES {
        if let Some(evicted) = r.lines.pop_front() {
            r.bytes -= evicted.len();
            r.dropped += 1;
        } else {
            break;
        }
    }
}

/// `2026-08-15T12:03:47.123Z` from the system clock — wall time, so a bundle correlates with
/// the host log it lands next to. No chrono dep; same civil-date derivation the host uses.
/// Lives here (not in a shell) because every ring FEEDER wants the same stamp: the session's
/// `ring_layer` and the Android client's logcat tee both prefix their lines with it.
pub fn wallclock() -> String {
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

/// The ring rendered as one text bundle, oldest first, prefixed by `header` (the shell's own
/// identity line — binary name, version, platform) and an eviction note when the ring wrapped.
pub fn render(header: &str) -> String {
    let r = RING.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = String::with_capacity(r.bytes + header.len() + 64);
    out.push_str(header);
    out.push('\n');
    if r.dropped > 0 {
        out.push_str(&format!(
            "… {} older lines evicted from the ring …\n",
            r.dropped
        ));
    }
    for line in &r.lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Upload the ring to the paired host `addr` and return the stored bundle id. Same transport +
/// trust as the library fetch: TLS client auth with the device identity, host pinned by
/// fingerprint. Errors reuse the library's classification (401/403 ⇒ `NotPaired`, a pin-verifier
/// rejection ⇒ `PinMismatch`), so the shell's existing error strings apply.
#[cfg(any(target_os = "linux", windows))]
pub fn send_to_host(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    header: &str,
) -> Result<String, crate::library::LibraryError> {
    use crate::library::LibraryError;
    let body = render(header);
    let agent = crate::library::agent(identity, pin)?;
    let url = format!(
        "{}/api/v1/client-logs",
        crate::library::base_url(addr, mgmt_port)
    );
    match agent
        .post(&url)
        .header("Content-Type", "text/plain; charset=utf-8")
        .send(body.as_bytes())
    {
        Ok(mut resp) => {
            let text = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| LibraryError::Unreachable(format!("read body: {e}")))?;
            let id = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("id")?.as_str().map(str::to_string))
                .unwrap_or_default();
            Ok(id)
        }
        Err(e) => Err(crate::library::classify(e)),
    }
}

/// Thin `tracing` layer feeding the ring — the source for "Send logs to host" in the session
/// binary and both desktop shells. Install it beside the visible (stderr/file) layer with its
/// own `LevelFilter::DEBUG`, NOT under the env filter: the whole point is that a field report
/// carries the diagnostics nobody thought to enable beforehand. Mirrors the host's
/// `log_capture::RingLayer`.
///
/// …which is exactly why it also has to keep OUT the chatter that would evict them. The ring
/// holds [`MAX_LINES`] lines. The vendored H.265 parser (`cros_codecs`, behind `pf-bitstream`)
/// DEBUG-logs its DPB bookkeeping — "Retaining pic POC", "Stored picture", "Set reference",
/// "Bumping POC", one `find_short_term_ref_by_poc` per reference — a dozen lines PER FRAME, so
/// at 120 fps the ring turns over in about three seconds. The 2026-08-17 field bundle from a
/// Steam Deck read `… 2037456 older lines evicted from the ring …` followed by 3.5 s of DPB
/// chatter: the whole 27-minute session, including the 10 s `audio playback buffer_ms=
/// underruns=` line three investigation rounds had been waiting for, was gone. A field ring
/// that a healthy decoder can flush is worse than no ring, because it looks like diagnostics
/// and carries none.
#[cfg(any(target_os = "linux", windows))]
pub struct RingLayer;

/// Targets whose DEBUG/TRACE output is steady-state per-frame chatter, not diagnostics. The ring
/// keeps their INFO-and-up. Prefix-matched on module-path boundaries, so `cros_codecs::codec::…`
/// is gated and a hypothetical `cros_codecs_probe` is not. Same shape as the host's
/// `log_capture::NOISY_DEBUG_TARGETS`.
#[cfg(any(target_os = "linux", windows))]
const NOISY_DEBUG_TARGETS: &[&str] = &["cros_codecs"];

#[cfg(any(target_os = "linux", windows))]
fn is_noisy_debug(target: &str) -> bool {
    NOISY_DEBUG_TARGETS.iter().any(|t| {
        target
            .strip_prefix(t)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

#[cfg(any(target_os = "linux", windows))]
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RingLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        use std::fmt::Write as _;
        use tracing::field::{Field, Visit};
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
        note(format!(
            "{} {:5} {} {}",
            wallclock(),
            meta.level().as_str(),
            meta.target(),
            v.0
        ));
    }
}

/// Forward a spawned session child's stderr: every line goes to OUR stderr (a dev terminal
/// keeps the interleaved output it always had) AND into the ring, so this shell's "Send logs
/// to host" bundle carries the session's whole receive/decode/present trail — the half of any
/// field report that matters. Line-buffered so child lines never interleave mid-line with the
/// shell's own. Returns immediately; the thread dies with the pipe (child exit).
///
/// The WinUI shell has its own forwarder (its tee also feeds the client log file); this one is
/// for callers whose stderr is the only other sink — `orchestrate`'s spawn.
#[cfg(any(target_os = "linux", windows))]
pub fn forward_child_stderr(stderr: impl std::io::Read + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("pf-session-log".into())
        .spawn(move || {
            use std::io::{BufRead as _, Write as _};
            let mut reader = std::io::BufReader::new(stderr);
            let mut line = String::new();
            while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                let _ = std::io::stderr().write_all(line.as_bytes());
                note(line.trim_end().to_string());
                line.clear();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ring is process-global; every test that writes it takes this, so a parallel run
    /// can't interleave lines into an order-sensitive assertion.
    static RING_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn ring_bounds_and_renders_with_eviction_note() {
        let _own = RING_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..(MAX_LINES + 10) {
            note(format!("line {i}"));
        }
        let text = render("punktfunk-client test");
        assert!(text.starts_with("punktfunk-client test\n"));
        assert!(text.contains("older lines evicted"));
        assert!(
            !text.contains("\nline 0\n"),
            "oldest line survived eviction"
        );
        assert!(text.ends_with(&format!("line {}\n", MAX_LINES + 9)));
        // A pathological line is truncated, not ring-flushing.
        note("x".repeat(10_000));
        let text = render("h");
        assert!(text.contains('…'));
    }

    /// The gate is a prefix match on module-path boundaries, nothing looser.
    #[cfg(any(target_os = "linux", windows))]
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
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn bridged_decoder_debug_is_dropped_and_the_audio_line_survives() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Layer;
        let _own = RING_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

        let text = render("test");
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
