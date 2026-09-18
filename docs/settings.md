# Client settings

The settings screen stays small. A setting is permanent surface: every client renders it, presets
and sync carry it, the docs explain it, and every later support thread asks whether it was set.
Add one only when it clearly makes streaming better for many players. This applies to the player
settings of every client — Android, Apple, desktop, the console shell, webOS and the browser.

## Before adding a setting

Answer these in order. The first "yes" ends the search for a setting.

1. **Can it just work?** Detect, measure, or pick the right default. A setting is not a way to skip
   a decision.
2. **Does the platform already say it?** Follow the OS: display scale, text size, theme, reduced
   motion. Do not add an app copy of a system setting.
3. **Is it a fix for one device, one link, or one report?** Fix the detection, or put it in logs
   or an operator env var. A per-device workaround is not a player setting.
4. **Can an existing control absorb it?** Reshape a control so it covers more players with no
   more choices, rather than adding a second one beside it.

A new setting also has to pass this: a player who does not know how streaming works reads the
label and its options and knows what to pick. If it needs a paragraph, it is not a player setting.

## Rules

- **Automatic means automatic.** No "Automatic, but…" modes and no tuning under an adaptive
  control. Bitrate is Automatic or a fixed rate.
- **Never change what the player picked.** Resolution and refresh stay as chosen; adapting moves
  only what Automatic owns.
- **No hidden state across sessions.** Behaviour that silently depends on an earlier session is
  a setting the player cannot see.
- **Operator and diagnostic knobs are not player settings.** They are `PUNKTFUNK_*` env vars or
  host settings.
- **One setting, every client that has the feature.** Shared key and wording (`docs/writing.md`
  §4b). A setting that exists on one client only needs a reason that platform alone has.
- **Ask first.** A PR that adds a player setting names the problem, who hits it, and what was
  tried to make it automatic — and has the maintainer's yes before it is opened.

## Examples

Good — **Aspect ratio + Resolution** (#1088). One extra field turned a single oversized list into
two short ones and covers every panel shape.

Bad — **Automatic with a limit** (#1175) added a third bitrate mode to understand; **per-host ABR
memory** (#1176) started good links at 2 Mbps from a quiet last session. Both reverted in #1207.
**Stepping the mode down** under Automatic (#1177) overrode the player's choice; closed.

Bad — **an overlay size picker** (#530) duplicated the OS display scale on four settings screens.
The picker was taken out before merge; the overlay follows the OS scale.
