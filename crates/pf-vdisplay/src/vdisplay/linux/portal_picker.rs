//! The line we feed xdg-desktop-portal-hyprland's **custom picker** to select an output headlessly.
//!
//! xdph has no headless source-selection API: it runs `screencopy:custom_picker_binary` and parses
//! one line from its stdout. The Hyprland backend points that at a shim which cats a per-session
//! file, and this module is the format of what goes in the file — a wire format with no schema, no
//! validation and no error report, whose only observable failure is a line in the portal's log.
//!
//! Declared unconditionally although only `hyprland.rs` calls it, for the reason `portal_config` and
//! `portal_cursor` give: this is pure string handling whose tests are the only place its behaviour
//! is checkable without a compositor in front of you, so they run on every platform's CI rather than
//! only on the leg that compiles `mod hyprland`. That is not hypothetical here — the bug below
//! shipped, and the one test that existed for it passed the whole time.

/// The picker line selecting monitor `name`: `[SELECTION]<flags>/<selection>`, with **empty flags**.
///
/// 🛑 THE `/` IS MANDATORY AND WE USED TO OMIT IT. xdph splits the line on the FIRST `/` into flags
/// and selection ([xdph 1.3.12] `src/shared/ScreencopyShared.cpp:86-87`):
///
/// ```text
/// const auto FLAGS = SELECTION.substr(0, SELECTION.find_first_of('/'));
/// const auto SEL   = SELECTION.substr(SELECTION.find_first_of('/') + 1);
/// ```
///
/// With no `/` anywhere, `find_first_of` returns `npos`, so `FLAGS` becomes the WHOLE payload — and
/// `SEL` becomes the whole payload too, purely because `npos + 1` wraps to `0`. The output name
/// therefore still parsed correctly, which is exactly why this survived: the only thing it broke was
/// the flag loop (`:89-94`), which then walked `screen:<name>` one character at a time —
///
/// ```text
/// [screencopy] unknown flag from share-picker: s
/// [screencopy] unknown flag from share-picker: c
/// [screencopy] unknown flag from share-picker: e     … one line per character
/// ```
///
/// — and, because `sc*r*een` contains an `r`, which is xdph's "allow restore token" flag, set
/// `data.allowToken = true`. xdph then answered every `Start` with a `restore_data` +
/// `persist_mode: 2` we never asked for (we request `PersistMode::DoNot`), which is the
/// `[screencopy] Sent restore token to …` on every single session in the field log.
///
/// The reference picker prints the separator unconditionally
/// (`hyprland-share-picker/main.cpp:133-136`):
///
/// ```text
/// std::cout << "[SELECTION]";
/// std::cout << (ALLOWTOKENBUTTON->isChecked() ? "r" : "");
/// std::cout << "/";
/// std::cout << "screen:" << outputName.toStdString() << "\n";
/// ```
///
/// so empty flags are spelled as a bare leading `/`, not as nothing at all.
///
/// ⚠️ This was NOT the cause of the "only the first stream works" stall — see `hyprland.rs`'s
/// `StopGuard` for that. The sessions that streamed fine logged the identical flag spam and the
/// identical restore token, so it never discriminated. It is a real bug on its own terms and nothing
/// more.
///
/// The trailing newline is equally load-bearing: xdph does `data.output.pop_back()` unconditionally
/// after `SEL.substr(7)` (`:96-100`), so without it the last character of the output name is eaten.
///
/// [xdph 1.3.12]: https://github.com/hyprwm/xdg-desktop-portal-hyprland/blob/v1.3.12/src/shared/ScreencopyShared.cpp
pub(crate) fn selection_line(name: &str) -> String {
    format!("[SELECTION]/screen:{name}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xdph 1.3.12's parser (`ScreencopyShared.cpp:82-100`), transcribed — including the `npos`
    /// arithmetic, which is the entire subtlety. Returns `(flags, output)`, or `None` where xdph
    /// would fall through to its interactive picker.
    ///
    /// Transcribed rather than asserted on the string, because the bug this catches is invisible in
    /// the string: the old line yielded the RIGHT OUTPUT NAME while handing xdph the whole selection
    /// as a FLAG STRING. Only running its parser tells the two apart.
    fn xdph_parse(picker_stdout: &str) -> Option<(String, String)> {
        // `if (!RETVAL.contains("[SELECTION]")) return data;` — a default `SSelectionData`, i.e.
        // TYPE_INVALID, which makes `SelectSources` fail.
        let marker = picker_stdout.find("[SELECTION]")?;
        let selection = &picker_stdout[marker + "[SELECTION]".len()..];
        // `substr(0, npos)` is the whole string, and `substr(npos + 1)` is `substr(0)` — also the
        // whole string. Unsigned wraparound, not a special case in xdph.
        let (flags, sel) = match selection.find('/') {
            Some(i) => (&selection[..i], &selection[i + 1..]),
            None => (selection, selection),
        };
        let name = sel.strip_prefix("screen:")?;
        // `data.output.pop_back()` — unconditional, hence the mandatory trailing newline.
        let mut output = name.to_string();
        output.pop();
        Some((flags.to_string(), output))
    }

    /// The three load-bearing parts of the line, pinned as bytes.
    #[test]
    fn the_line_carries_marker_empty_flags_separator_and_newline() {
        assert_eq!(selection_line("PF-1"), "[SELECTION]/screen:PF-1\n");
    }

    /// What xdph actually makes of our line: the exact output, and NO flags.
    #[test]
    fn xdph_reads_our_line_as_an_output_with_no_flags() {
        for name in ["PF-1", "PF-1620-1", "HDMI-A-1", "DP-2"] {
            let (flags, output) = xdph_parse(&selection_line(name)).expect("xdph parses our line");
            assert_eq!(output, name, "xdph must recover the exact output name");
            assert_eq!(flags, "", "we ask for no flags at all");
            assert!(
                !flags.contains('r'),
                "an `r` in the flags makes xdph hand back restore_data + persist_mode=2 we never \
                 requested (Screencopy.cpp:261-267)"
            );
        }
    }

    /// The regression itself, so it cannot come back by "simplifying" the leading `/` away: the line
    /// we used to send parsed the whole selection as flags, `r` included.
    #[test]
    fn without_the_separator_the_whole_selection_becomes_flags() {
        let (flags, output) = xdph_parse("[SELECTION]screen:PF-1620-1\n").expect("still parses");
        assert_eq!(
            output, "PF-1620-1",
            "the output name did survive — which is precisely why this hid for so long"
        );
        assert_eq!(
            flags, "screen:PF-1620-1\n",
            "…while the entire selection was handed to the flag loop"
        );
        assert!(
            flags.contains('r'),
            "the `r` of `sc*r*een` is xdph's allow-restore-token flag"
        );
    }

    /// Without the trailing newline xdph's unconditional `pop_back()` eats a character of the name —
    /// a silently wrong output, not an error.
    #[test]
    fn the_trailing_newline_is_what_pop_back_consumes() {
        assert!(selection_line("PF-1620-1").ends_with('\n'));
        let (_, truncated) = xdph_parse("[SELECTION]/screen:PF-1620-1").expect("parses");
        assert_eq!(
            truncated, "PF-1620-",
            "pop_back() takes the last real character"
        );
    }

    /// A line with no marker at all is xdph's documented empty-read fallback: it prompts instead. The
    /// shim relies on this when no session has written the selection file.
    #[test]
    fn an_empty_read_is_not_a_selection() {
        assert!(xdph_parse("").is_none());
        assert!(xdph_parse("screen:PF-1\n").is_none());
    }
}
