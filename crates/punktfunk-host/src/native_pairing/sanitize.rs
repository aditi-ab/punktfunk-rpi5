//! Sanitize a client-supplied device name before it is stored, listed, logged, or shown in the
//! pairing-approval UI. The name arrives on the wire from an *unpaired* device, so it is untrusted
//! (terminal-escape / control-char injection, bidi-override spoofing of a trusted-looking name) —
//! this is the one place that scrubs it. Split out of the `native_pairing` facade (plan §W5).

/// Sanitize a client-supplied device name before it's stored, listed, or logged. The name comes
/// straight off the wire (the `Hello`/`PairRequest` of an *unpaired* device), so it's untrusted: a
/// hostile LAN device could embed terminal escapes / control characters (log + console injection) or
/// bidi overrides (`U+202E` etc.) to make a malicious device *look* like a trusted one in the
/// approval UI. Strip C0/C1 controls and Unicode bidi/format controls, collapse whitespace, trim, and
/// cap the length; an empty/all-control name falls back to a fingerprint-derived label.
pub(crate) fn sanitize_device_name(name: &str, fp_hex: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '\t' || c == '\n' { ' ' } else { c })
        .filter(|&c| !c.is_control() && !is_spoofy_char(c))
        .collect();
    // Collapse internal whitespace runs, trim, cap at the wire limit.
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut trimmed = collapsed.as_str();
    while trimmed.len() > NAME_MAX {
        let mut cut = NAME_MAX;
        while !trimmed.is_char_boundary(cut) {
            cut -= 1;
        }
        trimmed = &trimmed[..cut];
    }
    let trimmed = trimmed.trim();
    if trimmed.is_empty() {
        format!("device {}", &fp_hex[..8.min(fp_hex.len())])
    } else {
        trimmed.to_string()
    }
}

/// A Unicode bidi/format control that could spoof or reorder a displayed name (an `RLO` making a
/// hostile device read like a trusted one). The canonical set — shared by every place that scrubs an
/// untrusted client name before display/storage (device names here, the stream marker) so the set
/// can't drift. Does NOT include C0/C1 controls; callers combine this with `char::is_control`.
pub(crate) fn is_spoofy_char(c: char) -> bool {
    ('\u{202A}'..='\u{202E}').contains(&c) // LRE..RLO/PDF
        || ('\u{2066}'..='\u{2069}').contains(&c) // LRI..PDI
        || c == '\u{200E}' // LRM
        || c == '\u{200F}' // RLM
        || c == '\u{061C}' // ALM
        || c == '\u{FEFF}' // BOM / zero-width no-break space
}

/// Max stored device-name length (matches the `Hello` wire cap, `quic::HELLO_NAME_MAX`).
const NAME_MAX: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_control_and_bidi() {
        // ANSI escape + newline + a bidi override that could spoof the displayed name.
        let dirty = "\u{1b}]0;evil\u{07}Good\nDevice\u{202E}xfp";
        let clean = sanitize_device_name(dirty, "deadbeef00");
        assert!(!clean.contains('\u{1b}') && !clean.contains('\n') && !clean.contains('\u{202E}'));
        // ESC dropped (']' survives), BEL dropped, '\n'→space (Good Device), RLO dropped (no space).
        assert_eq!(clean, "]0;evilGood Devicexfp");
        // All-control / empty → fingerprint-derived fallback.
        assert_eq!(
            sanitize_device_name("\u{1b}\u{07}", "deadbeef00"),
            "device deadbeef"
        );
        assert_eq!(sanitize_device_name("   ", "abc"), "device abc");
        // Over-long names cap at a char boundary.
        assert!(sanitize_device_name(&"x".repeat(200), "ab").len() <= 64);
    }
}
