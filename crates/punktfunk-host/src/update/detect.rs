//! **How was this host installed, and on which channel?** (design §4.1)
//!
//! The ladder itself, the version comparison and the per-kind command hints moved to
//! `pf-update-check` when the Linux client grew the same surface — the delivery channels are
//! the same ones, and two copies of "is this newer?" would have drifted. What stays here is
//! the host's binding of that shared machinery: [`Product::Host`], this binary's own version,
//! and the process-wide cache.
//!
//! Everything is re-exported under the names the rest of the host already uses, so call sites
//! (`detect::InstallKind::Apt`, `detect::is_newer(…)`, …) are unchanged.

// Only what the host actually names — an unused re-export is a warning, and CI runs
// `clippy -D warnings`. Tests reach for the rest through `pf_update_check::…` directly.
pub(crate) use pf_update_check::detect::InstallKind;
pub(crate) use pf_update_check::version::{is_newer, Channel};

use pf_update_check::detect::{classify as classify_shared, gather, Product};
use std::sync::OnceLock;

/// The process-wide answer, computed once.
pub(crate) fn detect() -> (InstallKind, Channel) {
    static DETECTED: OnceLock<(InstallKind, Channel)> = OnceLock::new();
    *DETECTED.get_or_init(|| {
        classify_shared(
            &gather(Product::Host, env!("PUNKTFUNK_VERSION")),
            Product::Host,
        )
    })
}

/// The host ladder over an explicit probe — the seam the tests use.
#[cfg(test)]
fn classify(p: &pf_update_check::detect::Probe) -> (InstallKind, Channel) {
    classify_shared(p, Product::Host)
}

/// The per-kind "how to update" command the console shows while (or instead of) an apply
/// path existing (design §5). One line, copy-pastable, no placeholders.
pub(crate) fn channel_hint(kind: InstallKind) -> String {
    pf_update_check::detect::update_command(kind, Product::Host)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared crate owns the ladder's own tests; this one guards the host's BINDING of
    /// it — that we ask as `Product::Host`, which is what makes a Deck build report the
    /// source-rebuild leg instead of the client's plain `source`.
    #[test]
    fn host_binding_reports_the_deck_source_leg() {
        let p = pf_update_check::detect::Probe {
            exe: "/home/deck/punktfunk/target-steamos/release/punktfunk-host".into(),
            home: Some("/home/deck".into()),
            ..Default::default()
        };
        assert_eq!(classify(&p).0, InstallKind::SteamosSource);
    }

    #[test]
    fn hints_name_the_host_package() {
        assert!(channel_hint(InstallKind::Apt).contains("punktfunk-host"));
        assert!(channel_hint(InstallKind::Sysext).contains("punktfunk-sysext update"));
    }
}
