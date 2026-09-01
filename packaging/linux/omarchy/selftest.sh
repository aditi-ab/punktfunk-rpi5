#!/usr/bin/env bash
#
# Self-check for the two pieces of `punktfunk-omarchy` that parse or generate a file the USER owns:
# the xdph picker restore (awk over ~/.config/hypr/xdph.conf) and the hooks.json generator. Both
# are reachable only on an Omarchy box, which is exactly why they need a check that runs anywhere.
#
#     bash packaging/linux/omarchy/selftest.sh
#
# Everything else in that script is systemctl/ufw/omarchy calls, which are the box's to answer.

set -euo pipefail
cd "$(dirname "$0")"

SCRIPT=./punktfunk-omarchy
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
fails=0

check() {  # check <name> <expected-file> <actual-file>
  if diff -u "$2" "$3" >/dev/null; then
    printf '  ok   %s\n' "$1"
  else
    printf '  FAIL %s\n' "$1"; diff -u "$2" "$3" | sed 's/^/       /'; fails=$((fails + 1))
  fi
}

# Source the script's functions without running its dispatcher: it dispatches on "$1", and "help"
# only prints. `set +e` around it because the script itself sets -e.
# shellcheck disable=SC1090
source "$SCRIPT" help >/dev/null

echo "xdph picker restore"

# 1. The Omarchy case: they had their own picker, we took it over, `remove` puts it back verbatim.
mkdir -p "$WORK/hypr"
cat > "$WORK/hypr/xdph.conf" <<'EOF'
screencopy {
    allow_token_by_default = true
    # punktfunk: previous custom_picker_binary = hyprland-preview-share-picker
    custom_picker_binary = /run/user/1000/punktfunk-xdph-picker.sh
}
EOF
cat > "$WORK/expected" <<'EOF'
screencopy {
    allow_token_by_default = true
    custom_picker_binary = hyprland-preview-share-picker
}
EOF
XDG_CONFIG_HOME="$WORK" restore_picker >/dev/null
check "their picker comes back and their other keys survive" "$WORK/expected" "$WORK/hypr/xdph.conf"

# 2. The key did not exist before us: restoring must REMOVE our line, not blank it or invent a value.
cat > "$WORK/hypr/xdph.conf" <<'EOF'
screencopy {
    # punktfunk: previous custom_picker_binary = (none)
    custom_picker_binary = /run/user/1000/punktfunk-xdph-picker.sh
}
EOF
printf 'screencopy {\n}\n' > "$WORK/expected"
XDG_CONFIG_HOME="$WORK" restore_picker >/dev/null
check "a key we invented is removed, not blanked" "$WORK/expected" "$WORK/hypr/xdph.conf"

# 3. A config that was never ours must come through byte-identical — this runs on every `remove`.
cat > "$WORK/hypr/xdph.conf" <<'EOF'
screencopy {
    custom_picker_binary = hyprland-preview-share-picker
}
EOF
cp "$WORK/hypr/xdph.conf" "$WORK/expected"
XDG_CONFIG_HOME="$WORK" restore_picker >/dev/null
check "a config without our marker is untouched" "$WORK/expected" "$WORK/hypr/xdph.conf"

# 4. No config at all: a no-op, and it must not CREATE one.
rm -f "$WORK/hypr/xdph.conf"
XDG_CONFIG_HOME="$WORK" restore_picker >/dev/null
if [[ -e "$WORK/hypr/xdph.conf" ]]; then
  printf '  FAIL restoring created a config that did not exist\n'; fails=$((fails + 1))
else
  printf '  ok   an absent config stays absent\n'
fi

echo "hooks.json"

# 5. Every combination of the two opt-ins must be valid JSON — the blocks are concatenated, so a
#    stray or missing comma between them is the failure mode.
for combo in "hooks_json" "idle_hooks_json" "hooks_json idle_hooks_json"; do
  # shellcheck disable=SC2086
  XDG_CONFIG_HOME="$WORK/fresh-${combo// /-}" write_hooks $combo >/dev/null
  f="$WORK/fresh-${combo// /-}/punktfunk/hooks.json"
  if python3 -c "import json,sys; d=json.load(open(sys.argv[1])); assert d['hooks'] and all('on' in h and 'run' in h for h in d['hooks'])" "$f"; then
    printf '  ok   valid JSON for [%s]\n' "$combo"
  else
    printf '  FAIL invalid JSON for [%s]\n' "$combo"; cat "$f"; fails=$((fails + 1))
  fi
done

# 6. An existing hooks.json is the operator's document: print, never overwrite.
mkdir -p "$WORK/mine/punktfunk"
echo '{"hooks":[{"on":"stream.started","webhook":"https://example.invalid/x"}]}' > "$WORK/mine/punktfunk/hooks.json"
cp "$WORK/mine/punktfunk/hooks.json" "$WORK/expected"
XDG_CONFIG_HOME="$WORK/mine" write_hooks hooks_json >/dev/null
check "an operator's own hooks.json is never overwritten" "$WORK/expected" "$WORK/mine/punktfunk/hooks.json"

echo "omarchy menu merge"

# The menu is a SINGLE JSONC document and one parse error drops every row the user owns, so the
# merge gets the same scrutiny as the picker restore.
menudir="$WORK/menu/omarchy/extensions"
mkdir -p "$menudir"
# The URL row matters: `//` inside a STRING is not a comment, and the first validator treated it
# as one — truncating the row mid-string and refusing to touch a file the user never broke.
cat > "$menudir/omarchy-menu.jsonc" <<'EOF'
{
  // a comment the user wrote
  "personal": {"icon":"","label":"Personal"},
  "personal.notes": {"icon":"󰎞","label":"Notes","action":"true"},
  "personal.site": {"icon":"󰖟","label":"Site","action":"omarchy-launch-webapp https://example.com"},
}
EOF
cp "$menudir/omarchy-menu.jsonc" "$WORK/menu-before"

XDG_CONFIG_HOME="$WORK/menu" setup_menu >/dev/null 2>&1
f="$menudir/omarchy-menu.jsonc"

if XDG_CONFIG_HOME="$WORK/menu" menu_is_valid "$f"; then
  printf '  ok   the merged menu still parses as JSONC\n'
else
  printf '  FAIL the merged menu does not parse\n'; cat "$f"; fails=$((fails + 1))
fi
if grep -q '"personal.notes"' "$f" && grep -q '"punktfunk-host.console"' "$f"; then
  printf "  ok   the user's rows survived and ours were added\n"
else
  printf "  FAIL rows lost in the merge\n"; fails=$((fails + 1))
fi

# Idempotent: a second run must not stack a second copy.
XDG_CONFIG_HOME="$WORK/menu" setup_menu >/dev/null 2>&1
n=$(grep -c '"punktfunk-host.console"' "$f")
if [[ "$n" == "1" ]]; then printf '  ok   re-running does not duplicate the block\n'
else printf '  FAIL block appears %s times after two runs\n' "$n"; fails=$((fails + 1)); fi

# And `remove` puts the file back exactly as the user had it.
XDG_CONFIG_HOME="$WORK/menu" remove_menu >/dev/null 2>&1
check "remove restores the user's file byte for byte" "$WORK/menu-before" "$f"

# A file that does not parse to begin with is not ours to repair — leave it untouched.
printf '{ this is not json\n' > "$f"
cp "$f" "$WORK/menu-broken"
XDG_CONFIG_HOME="$WORK/menu" setup_menu >/dev/null 2>&1
check "a config we cannot parse is left alone" "$WORK/menu-broken" "$f"

echo "app menu"

# `setup_webapp` must run to its END on a box with nothing of ours installed yet — no pre-rename
# entry, no applications dir at all. That is the ordinary first install, and it is where a bare
# `x=$(grep … | head -1)` under `set -e` + `pipefail` killed the script dead: the step header had
# already printed, so the operator saw "==> App menu" and then nothing, and every later step
# (plugin, menu, hooks, theme, status) silently never ran. Reaching `setup_menu` is the proof.
mkdir -p "$WORK/bin" "$WORK/webapp"
printf '#!/bin/sh\nexit 0\n' > "$WORK/bin/omarchy-webapp-install"
printf '#!/bin/sh\nexit 0\n' > "$WORK/bin/omarchy-webapp-remove"
chmod +x "$WORK/bin/omarchy-webapp-install" "$WORK/bin/omarchy-webapp-remove"

# A CHILD bash, not a subshell: `set -e` is suppressed for everything inside an `if` condition,
# subshells and called functions included, so an in-process call would pass even while the real
# `punktfunk-omarchy setup` dies. The child re-sources the script, so its own `set -euo pipefail`
# is what governs — the same shell state an operator gets.
if env HOME="$WORK/webapp" XDG_CONFIG_HOME="$WORK/webapp/config" PATH="$WORK/bin:$PATH" \
     bash -c "source $SCRIPT help >/dev/null 2>&1; setup_webapp" >/dev/null 2>&1 &&
   grep -q '"punktfunk-host.console"' "$WORK/webapp/config/omarchy/extensions/omarchy-menu.jsonc"
then
  printf '  ok   setup_webapp completes with no prior entry and reaches the menu\n'
else
  printf '  FAIL setup_webapp aborted before the menu on a clean box\n'; fails=$((fails + 1))
fi

echo "setup options"

# The installer asks for every optional step on its own screen and passes the answers here, so
# nothing in `setup` may prompt. A stray `read`, a missing case or a silently accepted value all
# put an Omarchy box back to two rounds of questions, which is what these four checks catch.
if grep -q 'read -r -p' "$SCRIPT"; then
  printf '  FAIL setup still prompts for something\n'; fails=$((fails + 1))
else
  printf '  ok   nothing in the script prompts\n'
fi

if (parse_setup_opts --toasts=0 --theme=0 >/dev/null 2>&1
    [[ "$OPT_TOASTS" == 0 && "$OPT_THEME" == 0 && "$OPT_IDLE" == 1 ]]); then
  printf '  ok   an option sets its own row and leaves the rest alone\n'
else
  printf '  FAIL parse_setup_opts did not apply the options\n'; fails=$((fails + 1))
fi

if (parse_setup_opts --toasts=maybe >/dev/null 2>&1); then
  printf '  FAIL a value that is not 1 or 0 was accepted\n'; fails=$((fails + 1))
else
  printf '  ok   a value that is not 1 or 0 is refused\n'
fi

if (parse_setup_opts --nonsense=1 >/dev/null 2>&1); then
  printf '  FAIL an unknown option was accepted\n'; fails=$((fails + 1))
else
  printf '  ok   an unknown option is refused\n'
fi

echo
if [[ $fails -eq 0 ]]; then echo "all checks passed"; else echo "$fails check(s) failed"; exit 1; fi
