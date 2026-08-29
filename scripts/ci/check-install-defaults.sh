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
SIL='ID=fedora\nVARIANT_ID=silverblue\nVERSION_ID=44\nPRETTY_NAME="Fedora Linux 44 (Silverblue)"\n'

# Desktop seat: only the couch/HTPC distros flip group + linger on.
defaults_case debian-desk  "$DEB"  desktop '' 'Full controller (joins the punktfunk group): no'
defaults_case debian-desk2 "$DEB"  desktop '' 'Third-party clients (Moonlight, Artemis): no'
defaults_case debian-desk3 "$DEB"  desktop '' 'Shared clipboard: no'
defaults_case debian-desk4 "$DEB"  desktop '' 'Start at boot with nobody logged in: no'
defaults_case fedora-desk  "$FED"  desktop '' 'Full controller (joins the punktfunk group): no'
defaults_case fedora-desk2 "$FED"  desktop '' 'Start at boot with nobody logged in: no'
defaults_case arch-desk    "$ARCH" desktop '' 'Full controller (joins the punktfunk group): no'
defaults_case arch-desk2   "$ARCH" desktop '' 'Start at boot with nobody logged in: no'
# Omarchy is a sit-at Arch flavour, not a couch/HTPC default — linger only if the session is seatless.
defaults_case omarchy-desk  "$OMA" desktop '' 'Full controller (joins the punktfunk group): no'
defaults_case omarchy-desk2 "$OMA" desktop '' 'Start at boot with nobody logged in: no'
defaults_case omarchy-ssh   "$OMA" headless '' 'Start at boot with nobody logged in: yes  (no graphical session)'

defaults_case bazzite-desk  "$BAZ" desktop '' 'Full controller (joins the punktfunk group): yes  (Bazzite — virtual Steam Deck pad)'
defaults_case bazzite-desk2 "$BAZ" desktop '' 'Start at boot with nobody logged in: yes  (Bazzite hosts are usually headless)'
defaults_case bazzite-desk3 "$BAZ" desktop '' 'Third-party clients (Moonlight, Artemis): no'
defaults_case bazzite-desk4 "$BAZ" desktop '' 'Shared clipboard: no'

defaults_case nobara-desk  "$NOB" desktop '' 'Full controller (joins the punktfunk group): yes  (Nobara — virtual Steam Deck pad)'
defaults_case nobara-desk2 "$NOB" desktop '' 'Start at boot with nobody logged in: yes  (Nobara hosts are usually headless)'
# Flags/env still win over the distro default.
defaults_case bazzite-flag "$BAZ" desktop '' 'Full controller (joins the punktfunk group): no' --no-punktfunk-group

# No graphical seat (SSH, CI, a pipe) → linger even on a generic distro.
defaults_case debian-ssh  "$DEB" headless '' 'Start at boot with nobody logged in: yes  (no graphical session)'
defaults_case debian-ssh2 "$DEB" headless '' 'Full controller (joins the punktfunk group): no'

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
defaults_case debian-gs2 "$DEB" desktop "$gsbin" 'Full controller (joins the punktfunk group): no'

# `ujust` and `rpm-ostree` are NOT couch-box tells. Bluefin and Aurora ship ujust; Silverblue
# and Kinoite are rpm-ostree (so the installer calls them FAMILY=sysext, same as Bazzite). All
# four are desktop workstations, and joining their users to the punktfunk group or enabling
# linger under --yes without asking is not what they installed. Only ID/ID_LIKE decides.
printf '#!/bin/sh\nexit 0\n' > "$ujbin/ujust"
chmod +x "$ujbin/ujust"
printf '#!/bin/sh\nexit 0\n' > "$ujbin/rpm-ostree"
chmod +x "$ujbin/rpm-ostree"
defaults_case debian-ujust "$DEB" desktop "$ujbin" 'Full controller (joins the punktfunk group): no'
defaults_case debian-ujust2 "$DEB" desktop "$ujbin" 'Start at boot with nobody logged in: no'
defaults_case silverblue  "$SIL" desktop "$ujbin" 'Full controller (joins the punktfunk group): no'
defaults_case silverblue2 "$SIL" desktop "$ujbin" 'Start at boot with nobody logged in: no'
# …and the narrowing must not cost Bazzite its own defaults, sysext family and all.
defaults_case bazzite-sysext "$BAZ" desktop "$ujbin" 'Full controller (joins the punktfunk group): yes  (Bazzite — virtual Steam Deck pad)'

# --no-start is "don't enable the services", not "don't configure": linger was promised in the
# summary, so it has to actually run. Every other case here passes --no-start, so without this
# one the matrix would happily certify a summary the run drops on the floor.
defaults_case bazzite-linger "$BAZ" desktop '' '+ sudo loginctl enable-linger'

# ------------------------------------------------------------------ run(): who gets `sudo -n`
# --dry-run returns before run() executes anything, so the matrix above is structurally blind to
# this. Lift the real function out of the script and drive it against a stub sudo instead. A
# terminal keeps sudo's own password prompt even under --yes — sudo reads /dev/tty, not stdin —
# and only a session with no terminal at all gets -n, where nothing can type a password anyway.
# TTY=/dev/null stands in for a terminal: run() only tests whether the variable is set, and a CI
# runner has no controlling terminal to open.
sudobin=$(mktemp -d)
trap 'rm -rf "$osr" "$gsbin" "$ujbin" "$sudobin"' EXIT
printf '#!/bin/sh\n[ "${1:-}" = -n ] && { echo SUDO-N; exit 0; }\necho SUDO-PLAIN\n' > "$sudobin/sudo"
chmod +x "$sudobin/sudo"

# name TTY-value YES expected-substring
run_case() {
    {
        echo 'die() { echo DIED; exit 1; }'
        awk '/^run\(\) \{/,/^\}/' scripts/install.sh
        printf "DRY=0; TTY=%s; YES=%s; DOCS_PAGE=x\nrun 'sudo true'\n" "$2" "$3"
    } > "$sudobin/harness.sh"
    got=$(PATH="$sudobin:$PATH" sh "$sudobin/harness.sh" 2>&1)
    case "$got" in
        *"$4"*) ;;
        *)
            echo "::error::run() with $1: expected '$4', got:"
            printf '%s\n' "$got" | sed 's/^/    /'
            fail=1 ;;
    esac
}
run_case 'a terminal and --yes'  /dev/null 1 SUDO-PLAIN
run_case 'a terminal, prompting' /dev/null 0 SUDO-PLAIN
run_case 'no terminal at all'    ''        1 SUDO-N

if [ "$fail" -ne 0 ]; then
    echo "installer default matrix failed"
    exit 1
fi
echo "installer default matrix ok"
