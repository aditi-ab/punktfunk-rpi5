#!/bin/sh
# Docs drift gates (docs-and-onboarding-overhaul WP1). The docs rotted through four unchecked
# duplication channels; these are the cheap textual halves of closing them. No cargo, no bun
# install — pure git grep, so the gate runs on every push in seconds (the deep half, regenerating
# the OpenAPI spec from the host binary, lives in ci.yml's `rust` job where the build already
# exists).
#
#   1. docs-site/public/openapi.json must be a byte-for-byte copy of api/openapi.json. The
#      snapshot is a manual `cp` (docs-site/README.md); it sat stale on main for weeks once,
#      publishing an /api reference missing the whole self-update surface.
#   2. Every PUNKTFUNK_* variable the docs mention must still exist somewhere in the tree
#      (docs describing a removed knob is drift a reader pays for). Historical records —
#      docs/releases/, CHANGELOG.md — don't count as existence: a knob that lives only in old
#      release notes is gone.
#   3. Ratchet: the count of PUNKTFUNK_* env vars in code that the docs never mention must not
#      grow. 300+ internal/debug knobs are deliberately undocumented today, so this can't be a
#      hard list — but a NEW knob must either be documented in docs-site (configuration.md or
#      the page owning its feature) or the baseline below raised in the same commit, making
#      "undocumented" a decision instead of an accident. Shrink the gap? Lower the baseline.
#   4. Every command the host-cli.md tables list must still exist as a string literal in
#      crates/punktfunk-host (same "docs describing removed things" class as gate 2).
#   5. data/platforms.json (the single source for install/port facts that docs, the website
#      download page and the guided installer consume) must parse, and docs-site/src/data/
#      platforms.json — the snapshot the <Install/> and <Ports/> MDX components render from
#      (the docs Docker build context is docs-site/ alone) — must be a byte copy of it, same
#      rule as gate 1.
#   6. scripts/install.sh (the guided installer, WP4) runs the install lines platforms.json
#      states — every `install` line of an apt/pacman/dnf/sysext host platform must appear in
#      the script verbatim (it edits channel/group into the string at run time, never the
#      literal), and the script must parse under sh.
#   7. The installer under --dry-run against faked os-release files detects every family it claims
#      to (and --uninstall prints each family's removal) — the committed half of the manual
#      16-file matrix PR #345 was verified with. Needs curl on PATH (the script's own prerequisite).
#
# Textual gates, so textual limits: gate 2/3 match token spelling, not env reads — a var name in
# a code comment counts as "exists", and a quoted constant that isn't an env var counts toward
# the ratchet. Both err toward false calm on removal and a one-line baseline bump on addition,
# which is the cheap side to be wrong on.

set -u
LC_ALL=C
export LC_ALL
cd "$(dirname "$0")/../.." || exit 2

fail=0
tmp="${TMPDIR:-/tmp}/docs-drift.$$"
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT

# ---------------------------------------------------------------- gate 1: openapi snapshot
if [ "$(cksum < api/openapi.json)" != "$(cksum < docs-site/public/openapi.json)" ]; then
    echo "::error::docs-site/public/openapi.json is not a copy of api/openapi.json — re-sync it:"
    echo "  cp api/openapi.json docs-site/public/openapi.json"
    fail=1
fi

# ---------------------------------------------------------------- gate 2: docs env vars exist
git grep -ohE 'PUNKTFUNK_[A-Z0-9_]+' -- docs-site/content | sort -u > "$tmp/docs-vars"
while IFS= read -r var; do
    if ! git grep -qF "$var" -- ':!docs-site' ':!docs/releases' ':!CHANGELOG.md'; then
        echo "::error::docs-site documents $var but nothing outside the docs mentions it — the knob was removed or renamed; fix the docs page"
        fail=1
    fi
done < "$tmp/docs-vars"

# ---------------------------------------------------------------- gate 3: undocumented ratchet
# Quoted occurrences only: env reads are quoted string literals; bare identifiers are
# Rust constants / C symbols, not knobs. The committed baseline enumerates today's undocumented
# set so a violation names exactly the new knob.
baseline=scripts/ci/docs-undocumented-env-baseline.txt
sort -u "$baseline" > "$tmp/baseline"   # re-sort under our LC_ALL=C, whatever locale wrote it
git grep -ohE '"PUNKTFUNK_[A-Z0-9_]+"' -- ':!docs-site' ':!*.md' | tr -d '"' | sort -u > "$tmp/code-vars"
comm -23 "$tmp/code-vars" "$tmp/docs-vars" > "$tmp/undocumented"
comm -23 "$tmp/undocumented" "$tmp/baseline" > "$tmp/new-undocumented"
if [ -s "$tmp/new-undocumented" ]; then
    echo "::error::new PUNKTFUNK_* vars are neither documented in docs-site nor in the baseline:"
    sed 's/^/  /' "$tmp/new-undocumented"
    echo "Document each in docs-site (configuration.md or the page owning the feature), or — for a deliberately internal knob — add it to $baseline in the same commit."
    fail=1
fi
comm -13 "$tmp/undocumented" "$tmp/baseline" > "$tmp/stale-baseline"
if [ -s "$tmp/stale-baseline" ]; then
    echo "baseline entries no longer undocumented (removed or now documented) — prune them from $baseline:"
    sed 's/^/  /' "$tmp/stale-baseline"
fi

# ---------------------------------------------------------------- gate 4: documented CLI exists
# First table cell of every row in host-cli.md: subcommands, sub-actions and flags. Multi-word
# cells (flag + argument) are skipped — they don't map to one string literal.
grep -E '^\|' docs-site/content/docs/host-cli.md | awk -F'|' '{print $2}' \
    | grep -oE '`[a-z0-9-]+`|`--[a-z-]+`' | tr -d '`' | sort -u > "$tmp/cli-cmds"
while IFS= read -r cmd; do
    if ! git grep -qF "\"$cmd\"" -- crates/punktfunk-host; then
        echo "::error::host-cli.md documents \`$cmd\` but crates/punktfunk-host has no \"$cmd\" literal — removed or renamed; fix the docs page"
        fail=1
    fi
done < "$tmp/cli-cmds"

# ---------------------------------------------------------------- gate 5: platforms.json parses
# Explicit try/exit: `bun -e` (1.3.x) exits 0 on an uncaught JSON.parse throw, so relying on
# the default uncaught-exception exit code silently disarms the gate.
json_check='try{JSON.parse(require("fs").readFileSync("data/platforms.json","utf8"))}catch(e){console.error(e.message);process.exit(1)}'
if [ ! -f data/platforms.json ]; then
    echo "::error::data/platforms.json is missing"
    fail=1
elif command -v bun >/dev/null 2>&1; then
    bun -e "$json_check" || { echo "::error::data/platforms.json is not valid JSON"; fail=1; }
elif command -v node >/dev/null 2>&1; then
    node -e "$json_check" || { echo "::error::data/platforms.json is not valid JSON"; fail=1; }
fi
if [ "$(cksum < data/platforms.json)" != "$(cksum < docs-site/src/data/platforms.json 2>/dev/null)" ]; then
    echo "::error::docs-site/src/data/platforms.json is not a copy of data/platforms.json — re-sync it:"
    echo "  cp data/platforms.json docs-site/src/data/platforms.json"
    fail=1
fi

# ---------------------------------------------------------------- gate 6: installer quotes platforms.json
if ! sh -n scripts/install.sh; then
    echo "::error::scripts/install.sh does not parse under sh"
    fail=1
fi
installer_check='const fs=require("fs");const p=JSON.parse(fs.readFileSync("data/platforms.json","utf8"));const sh=fs.readFileSync("scripts/install.sh","utf8");let bad=0;for(const x of p.platforms){if(x.installs!=="host"||!["apt","pacman","dnf","sysext"].includes(x.packageManager))continue;for(const line of x.install||[]){if(!sh.includes(line)){console.error(`::error::scripts/install.sh no longer carries platforms.json\x27s ${x.id} install line verbatim: ${line}`);bad=1}}}process.exit(bad)'
if command -v bun >/dev/null 2>&1; then
    bun -e "$installer_check" || fail=1
elif command -v node >/dev/null 2>&1; then
    node -e "$installer_check" || fail=1
fi

# ---------------------------------------------------------------- gate 7: installer detection matrix (--dry-run)
# Faked os-release files through the real script, nothing executed: each family must be detected
# and print its own package-manager line, both for the install and for --uninstall; the unsupported
# ones must stop with their pointer. A fix to the installer adds its case here.
osr=$(mktemp -d)
installer_case() {   # name os-release-body expected-substring [extra args...]
    name=$1; printf '%b' "$2" > "$osr/$name"; want=$3; shift 3
    out=$(PUNKTFUNK_INSTALL_OS_RELEASE="$osr/$name" sh scripts/install.sh --dry-run --yes --no-start "$@" 2>&1)
    case "$out" in *"$want"*) ;; *)
        echo "::error::scripts/install.sh --dry-run $* on a fake $name os-release did not print '$want':"
        printf '%s\n' "$out" | sed 's/^/    /'
        fail=1 ;;
    esac
}
installer_case debian   'ID=debian\nVERSION_ID=13\n'                    'sudo apt install -y punktfunk-host punktfunk-web punktfunk-scripting'
installer_case ubuntu   'ID=ubuntu\nID_LIKE=debian\nVERSION_ID=26.04\n'  'sudo apt install -y punktfunk-host punktfunk-web punktfunk-scripting'
installer_case mint22   'ID=linuxmint\nID_LIKE="ubuntu debian"\nVERSION_ID=22.1\n' 'cannot host'
installer_case fedora   'ID=fedora\nVERSION_ID=44\n'                    'sudo dnf install -y punktfunk punktfunk-web punktfunk-scripting'
installer_case fedora43 'ID=fedora\nVERSION_ID=43\n'                    '/rpm/bazzite'
installer_case arch     'ID=arch\n'                                    'sudo pacman -Syu --noconfirm punktfunk-host punktfunk-web punktfunk-scripting'
installer_case cachyos  'ID=cachyos\nID_LIKE="arch"\n'                  'sudo pacman -Syu --noconfirm punktfunk-host punktfunk-web punktfunk-scripting'
installer_case bazzite  'ID=bazzite\nID_LIKE="fedora"\nVERSION_ID=43\n' 'punktfunk-sysext.sh install'
installer_case nixos    'ID=nixos\n'                                   'docs/nixos'
installer_case steamos  'ID=steamos\nID_LIKE=arch\n'                    'docs/steamos-host'
installer_case gentoo   'ID=gentoo\n'                                  'build-from-source'
installer_case debian-rm 'ID=debian\nVERSION_ID=13\n'                   'sources.list.d/punktfunk.list' --uninstall
installer_case fedora-rm 'ID=fedora\nVERSION_ID=44\n'                   'yum.repos.d/punktfunk.repo' --uninstall
installer_case arch-rm   'ID=arch\n'                                   '/etc/pacman.conf' --uninstall
installer_case bazzite-rm 'ID=bazzite\nID_LIKE="fedora"\nVERSION_ID=43\n' 'punktfunk-sysext remove' --uninstall

# ---------------------------------------------------------------- gate 8: channel switching
# A box that already has all three binaries, on a repo config naming one channel, told --channel
# <the other>: it must rewrite the repo AND re-resolve in a direction the package manager would
# otherwise refuse (canary is always a minor ahead of stable, so canary->stable is a downgrade).
# Two fakes make that reachable under --dry-run: stub binaries on PATH for the "already installed"
# probe, and PUNKTFUNK_INSTALL_ETC pointing at the repo config the box is supposedly on.
sw=$(mktemp -d); mkdir -p "$sw/bin"
for b in punktfunk-host punktfunk-web-server punktfunk-scripting; do
    printf '#!/bin/sh\necho 0.0.0-test\n' > "$sw/bin/$b"; chmod +x "$sw/bin/$b"
done
switch_case() {   # name os-release-body config-path config-body expected-substring [extra args...]
    name=$1; printf '%b' "$2" > "$osr/$name"
    mkdir -p "$sw/$name/$(dirname "$3")"; printf '%b' "$4" > "$sw/$name/$3"; want=$5; shift 5
    out=$(PATH="$sw/bin:$PATH" PUNKTFUNK_INSTALL_OS_RELEASE="$osr/$name" PUNKTFUNK_INSTALL_ETC="$sw/$name" \
          sh scripts/install.sh --dry-run --yes --no-start "$@" 2>&1)
    case "$out" in *"$want"*) ;; *)
        echo "::error::scripts/install.sh --dry-run $* on an installed $name box did not print '$want':"
        printf '%s\n' "$out" | sed 's/^/    /'
        fail=1 ;;
    esac
}
APT_LIST=etc/apt/sources.list.d/punktfunk.list
DEB13='ID=debian\nVERSION_ID=13\n'
switch_case apt-up   "$DEB13" "$APT_LIST" 'deb [x] https://git.unom.io/api/packages/unom/debian stable main\n' \
    'debian canary main' --channel canary
switch_case apt-down "$DEB13" "$APT_LIST" 'deb [x] https://git.unom.io/api/packages/unom/debian canary main\n' \
    '--allow-downgrades' --channel stable
# The regression this gate exists for. A canary box missing one of the three packages, re-run with
# no --channel at all: the missing ones must come from CANARY. Letting the flag's stable default
# win there rewrites the repo and drags the whole box back a channel without ever saying so.
mkdir -p "$sw/partial/bin" "$sw/partial/$(dirname "$APT_LIST")"
printf '#!/bin/sh\necho 0.0.0-test\n' > "$sw/partial/bin/punktfunk-host"
chmod +x "$sw/partial/bin/punktfunk-host"
printf 'deb [x] https://git.unom.io/api/packages/unom/debian canary main\n' > "$sw/partial/$APT_LIST"
printf '%b' "$DEB13" > "$osr/apt-partial"
out=$(PATH="$sw/partial/bin:$PATH" PUNKTFUNK_INSTALL_OS_RELEASE="$osr/apt-partial" \
      PUNKTFUNK_INSTALL_ETC="$sw/partial" sh scripts/install.sh --dry-run --yes --no-start 2>&1)
case "$out" in *'debian canary main'*) ;; *)
    echo "::error::scripts/install.sh with no --channel, on a canary box missing packages, did not stay on canary:"
    printf '%s\n' "$out" | sed 's/^/    /'
    fail=1 ;;
esac
switch_case dnf-up   'ID=fedora\nVERSION_ID=44\n' etc/yum.repos.d/punktfunk.repo \
    'baseurl=https://git.unom.io/api/packages/unom/rpm/fedora-44\n' 'distro-sync' --channel canary
switch_case pac-down 'ID=arch\n' etc/pacman.conf '[punktfunk-canary]\nServer = x\n' \
    "/^Server = /d' /etc/pacman.conf" --channel stable
switch_case sys-down 'ID=bazzite\nID_LIKE="fedora"\nVERSION_ID=43\n' etc/punktfunk-sysext.conf 'CHANNEL=canary\n' \
    'punktfunk-sysext.sh install --channel stable' --channel stable
rm -rf "$sw" "$osr"

exit "$fail"
