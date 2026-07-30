//! Release channels and the "is the manifest newer than me?" comparison.
//!
//! The hard part is canary: every packaging channel spells the same CI build differently
//! (`0.23.0~ci10250.gab12cd34` on deb, `0.23.0-0.ci10250.g…` on rpm, a zero-padded pkgrel on
//! pacman, `0.23.10250` on Windows/decky). Comparing those strings to each other is
//! meaningless, so canary compares `(major, minor)` and then the **CI run number**, which is
//! the one monotonic axis every channel carries. Stable compares the plain triple.
//!
//! Everything here is definitive-or-false: an unparseable pair never flags an update. A UI
//! still shows both version strings — the badge just doesn't light up on guesswork.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Canary,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Canary => "canary",
        }
    }
}

/// Leading `major.minor.patch` of a version string, ignoring any suffix (`~ci…`, `-1`, `+…`).
pub fn triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty());
    // Split on any non-digit: "0.23.0~ci10250.gab" → 0,23,0,10250… — take the first three
    // ONLY if the string actually starts with digits (else it's not a version at all).
    if !v.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

/// The CI run number embedded in a canary version string, wherever the channel's format hid
/// it: `0.23.0~ci10250.g<sha>` (deb), `0.23.0-0.ci10250.g<sha>` (rpm), `0.23.10250`
/// (Windows/decky style, run-as-patch). A stable string yields `None`.
pub fn canary_run(version: &str) -> Option<u64> {
    // `ci` immediately followed by digits, anywhere.
    let mut rest = version;
    while let Some(pos) = rest.find("ci") {
        let digits: String = rest[pos + 2..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return digits.parse().ok();
        }
        rest = &rest[pos + 2..];
    }
    match triple(version) {
        Some((_, _, patch)) if patch >= 1000 => Some(patch),
        _ => None,
    }
}

/// Is the manifest's release newer than `current`? See the module docs for why canary
/// compares run numbers rather than patch fields.
pub fn is_newer(
    manifest_version: &str,
    manifest_ci_run: Option<u64>,
    current: &str,
    channel: Channel,
) -> bool {
    let (Some(m), Some(c)) = (triple(manifest_version), triple(current)) else {
        return false;
    };
    match channel {
        Channel::Stable => m > c,
        Channel::Canary => {
            if (m.0, m.1) != (c.0, c.1) {
                return (m.0, m.1) > (c.0, c.1);
            }
            let manifest_run = manifest_ci_run.or_else(|| canary_run(manifest_version));
            match (manifest_run, canary_run(current)) {
                (Some(mr), Some(cr)) => mr > cr,
                _ => false,
            }
        }
    }
}

/// Windows canary installers are versioned `M.m.<run>` where `<run>` is a 4+ digit CI run
/// number; stable patch numbers stay small. Heuristic, documented in the plan (R10).
pub fn windows_channel_of(version: &str) -> Channel {
    match triple(version) {
        Some((_, _, patch)) if patch >= 1000 => Channel::Canary,
        _ => Channel::Stable,
    }
}

/// `CHANNEL=canary` in a shell-style conf (the sysext updater's own format).
pub fn conf_channel(conf: &str) -> Option<Channel> {
    for line in conf.lines() {
        if let Some(v) = line.trim().strip_prefix("CHANNEL=") {
            return Some(match v.trim() {
                "canary" => Channel::Canary,
                _ => Channel::Stable,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triples() {
        assert_eq!(triple("0.23.0"), Some((0, 23, 0)));
        assert_eq!(triple("0.23.0~ci10250.gab12cd34"), Some((0, 23, 0)));
        assert_eq!(triple("0.23.10250"), Some((0, 23, 10250)));
        assert_eq!(triple("garbage"), None);
        assert_eq!(triple("1.2"), None);
    }

    #[test]
    fn canary_runs() {
        assert_eq!(canary_run("0.23.0~ci10250.gab12cd34"), Some(10250));
        assert_eq!(canary_run("0.23.0-0.ci777.g12345678"), Some(777));
        assert_eq!(canary_run("0.23.10250"), Some(10250)); // run-as-patch (Windows/decky)
        assert_eq!(canary_run("0.23.0"), None); // stable string
        assert_eq!(canary_run("0.23.0-1"), None);
    }

    #[test]
    fn newer_stable() {
        assert!(is_newer("0.23.0", None, "0.22.2", Channel::Stable));
        assert!(!is_newer("0.22.2", None, "0.22.2", Channel::Stable));
        assert!(!is_newer("0.22.1", None, "0.22.2", Channel::Stable)); // downgrade never flags
        assert!(!is_newer("not-a-version", None, "0.22.2", Channel::Stable));
    }

    #[test]
    fn newer_canary_compares_runs_not_patch() {
        // deb canary current vs Windows-style manifest version, same run ⇒ NOT newer,
        // even though a naive triple compare says 10250 > 0.
        assert!(!is_newer(
            "0.23.10250",
            Some(10250),
            "0.23.0~ci10250.gab12cd34",
            Channel::Canary
        ));
        assert!(is_newer(
            "0.23.10251",
            Some(10251),
            "0.23.0~ci10250.gab12cd34",
            Channel::Canary
        ));
        // Minor bump wins outright.
        assert!(is_newer(
            "0.24.100",
            Some(100),
            "0.23.0~ci10250.g12",
            Channel::Canary
        ));
        // No run extractable on either side ⇒ conservative false.
        assert!(!is_newer("0.23.10250", None, "0.23.0", Channel::Canary));
    }

    #[test]
    fn windows_channel_heuristic() {
        assert_eq!(windows_channel_of("0.22.2"), Channel::Stable);
        assert_eq!(windows_channel_of("0.23.10118"), Channel::Canary);
    }

    #[test]
    fn conf_channels() {
        assert_eq!(conf_channel("CHANNEL=canary\n"), Some(Channel::Canary));
        assert_eq!(
            conf_channel("# a comment\nCHANNEL=stable"),
            Some(Channel::Stable)
        );
        assert_eq!(conf_channel("KEEP=6\n"), None);
    }
}
