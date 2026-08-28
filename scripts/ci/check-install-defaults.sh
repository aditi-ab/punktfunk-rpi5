#!/bin/sh
# Derived defaults for the guided installer (scripts/install.sh, issue #431).
# Faked os-release + --dry-run --yes --no-start: each family must print the summary that
# --yes would actually take, and that summary must appear before the first sudo.
# DISPLAY / WAYLAND_DISPLAY / XDG_SESSION_TYPE are pinned per case so a graphical seat on
# the machine running the gate cannot leak into a "headless" assertion.
set -u
cd "$(dirname "$0")/../.." || exit 2

fail=0
osr=$(mktemp -d)
gsbin=$(mktemp -d)
ujbin=$(mktemp -d)
trap 'rm -rf "$osr" "$gsbin" "$ujbin"' EXIT

# name os-release-body seat(desktop|headless) extra-PATH-dir expected-substring [install.sh args...]
defaults_case() {
    name=$1
    printf '%b' "$2" > "$osr/$name"
    seat=$3
    extra=$4
    want=$5
    shift 5
    case "$seat" in
        desktop)  _env="DISPLAY=:0 WAYLAND_DISPLAY= XDG_SESSION_TYPE=x11" ;;
        headless) _env="DISPLAY= WAYLAND_DISPLAY= XDG_SESSION_TYPE=" ;;
        *) echo "::error::defaults_case $name: seat must be desktop or headless, got '$seat'"; fail=1; return ;;
    esac
    # $_env is expanded on purpose: the seat pins must override a graphical session on the
    # machine running the gate. Quoted "$@" are extra install.sh flags (e.g. --no-punktfunk-group).
    out=$(env $_env PATH="${extra:+$extra:}$PATH" \
          PUNKTFUNK_INSTALL_OS_RELEASE="$osr/$name" \
          sh scripts/install.sh --dry-run --yes --no-start "$@" 2>&1) || {
        echo "::error::scripts/install.sh --dry-run --yes --no-start on fake $name ($seat) exited $?"
        printf '%s\n' "$out" | sed 's/^/    /'
        fail=1
        return
    }
    case "$out" in
        *"$want"*) ;;
        *)
            echo "::error::scripts/install.sh --dry-run on fake $name ($seat) did not print '$want':"
            printf '%s\n' "$out" | sed 's/^/    /'
            fail=1
            return ;;
    esac
    # Summary is the audit trail for --yes; it has to land before any privileged command.
    # A re-run on a box that already has the host may print no sudo at all — Choices must
    # still appear; when sudo does print, it must not come first.
    if ! printf '%s\n' "$out" | awk '
        /Choices \(nothing below has run yet\)/ { c=NR }
        index($0, "  + sudo") && !s { s=NR }
        END { if (!c) exit 1; if (s && c > s) exit 1 }
    '; then
        echo "::error::scripts/install.sh --dry-run on fake $name ($seat): Choices must print before the first '+ sudo':"
        printf '%s\n' "$out" | sed 's/^/    /'
        fail=1
    fi
}

DEB='ID=debian\nVERSION_ID=13\nPRETTY_NAME="Debian GNU/Linux 13"\n'
FED='ID=fedora\nVERSION_ID=44\nPRETTY_NAME="Fedora Linux 44"\n'
ARCH='ID=arch\nPRETTY_NAME="Arch Linux"\n'
OMA='ID=omarchy\nID_LIKE=arch\nVERSION_ID=4.0.1\nPRETTY_NAME="Omarchy"\n'
BAZ='ID=bazzite\nID_LIKE="fedora"\nVERSION_ID=43\nPRETTY_NAME="Bazzite"\n'
NOB='ID=nobara\nID_LIKE="fedora"\nVERSION_ID=43\nPRETTY_NAME="Nobara Linux"\n'

# Desktop seat: only the couch/HTPC distros flip group + linger on.
defaults_case debian-desk  "$DEB"  desktop '' 'Full controller: no'
defaults_case debian-desk2 "$DEB"  desktop '' 'Third-party clients (Moonlight, Artemis): no'
defaults_case debian-desk3 "$DEB"  desktop '' 'Shared clipboard: no'
defaults_case debian-desk4 "$DEB"  desktop '' 'Start at boot with nobody logged in: no'
defaults_case fedora-desk  "$FED"  desktop '' 'Full controller: no'
defaults_case fedora-desk2 "$FED"  desktop '' 'Start at boot with nobody logged in: no'
defaults_case arch-desk    "$ARCH" desktop '' 'Full controller: no'
defaults_case arch-desk2   "$ARCH" desktop '' 'Start at boot with nobody logged in: no'
# Omarchy is a sit-at Arch flavour, not a couch/HTPC default — linger only if the session is seatless.
defaults_case omarchy-desk  "$OMA" desktop '' 'Full controller: no'
defaults_case omarchy-desk2 "$OMA" desktop '' 'Start at boot with nobody logged in: no'
defaults_case omarchy-ssh   "$OMA" headless '' 'Start at boot with nobody logged in: yes  (no graphical session)'

defaults_case bazzite-desk  "$BAZ" desktop '' 'Full controller: yes  (Bazzite — virtual Steam Deck pad)'
defaults_case bazzite-desk2 "$BAZ" desktop '' 'Start at boot with nobody logged in: yes  (Bazzite hosts are usually headless)'
defaults_case bazzite-desk3 "$BAZ" desktop '' 'Third-party clients (Moonlight, Artemis): no'
defaults_case bazzite-desk4 "$BAZ" desktop '' 'Shared clipboard: no'

defaults_case nobara-desk  "$NOB" desktop '' 'Full controller: yes  (Nobara — virtual Steam Deck pad)'
defaults_case nobara-desk2 "$NOB" desktop '' 'Start at boot with nobody logged in: yes  (Nobara hosts are usually headless)'
# Flags/env still win over the distro default.
defaults_case bazzite-flag "$BAZ" desktop '' 'Full controller: no' --no-punktfunk-group

# No graphical seat (SSH, CI, a pipe) → linger even on a generic distro.
defaults_case debian-ssh  "$DEB" headless '' 'Start at boot with nobody logged in: yes  (no graphical session)'
defaults_case debian-ssh2 "$DEB" headless '' 'Full controller: no'

# Active Sunshine-family host (detect-conflicts exit 1) → Moonlight compat defaults on.
# A dormant leftover is not enough — that is the same split detect-conflicts uses.
cat > "$gsbin/punktfunk-host" <<'EOF'
#!/bin/sh
if [ "${1:-}" = detect-conflicts ]; then
    echo "Sunshine is running"
    exit 1
fi
echo 0.0.0-test
EOF
chmod +x "$gsbin/punktfunk-host"
defaults_case debian-gs "$DEB" desktop "$gsbin" 'Third-party clients (Moonlight, Artemis): yes  (Sunshine/Apollo already on this box)'
defaults_case debian-gs2 "$DEB" desktop "$gsbin" 'Full controller: no'

# ujust is how Bazzite-shaped boxes that aren't ID=bazzite still identify.
printf '#!/bin/sh\nexit 0\n' > "$ujbin/ujust"
chmod +x "$ujbin/ujust"
defaults_case debian-ujust "$DEB" desktop "$ujbin" 'Full controller: yes  (Game Mode / HTPC box — virtual Steam Deck pad)'
defaults_case debian-ujust2 "$DEB" desktop "$ujbin" 'Start at boot with nobody logged in: yes  (Game Mode / HTPC box)'

if [ "$fail" -ne 0 ]; then
    echo "installer default matrix failed"
    exit 1
fi
echo "installer default matrix ok"
