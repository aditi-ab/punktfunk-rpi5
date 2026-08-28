#!/bin/sh
# punktfunk guided host installer — PREVIEW.
#
#   curl -fsSL https://punktfunk.unom.io/install.sh | sh
#   curl -fsSLO https://punktfunk.unom.io/install.sh && sh install.sh --help   # read it first
#
# Zero-to-streaming for a Linux host: detect the distro → add the package repo → install → deal
# with a Sunshine/Apollo/Vibeshine already on the box (move one port) → groups → options →
# firewall → start the services → verify → print how to pair. Plain POSIX sh, `read` prompts,
# no TUI. Every prompt has a default so `--yes` (or no terminal) runs unattended.
#
# This is WP4 of the docs-and-onboarding overhaul and is labelled PREVIEW on purpose: the per-distro
# docs pages (https://docs.punktfunk.unom.io/docs/install) remain the documented default until it
# has mileage. The install commands it runs are the ones data/platforms.json states — verbatim, and
# CI (scripts/ci/check-docs-drift.sh) fails if they drift apart. Windows (winget/installer), the
# clients, NixOS and SteamOS have their own paths; this script points at them and stops.
#
# Exit codes: 0 done · 1 unsupported system / a step failed · 2 bad usage.
set -u

DOCS=https://docs.punktfunk.unom.io/docs
USER=${USER:-$(id -un)}; export USER

# ---------------------------------------------------------------------------- options
YES=${PUNKTFUNK_INSTALL_YES:-0}
CHANNEL=${PUNKTFUNK_INSTALL_CHANNEL:-stable}
GAMESTREAM=${PUNKTFUNK_INSTALL_GAMESTREAM:-}     # 1/0, empty = ask (default no)
CLIPBOARD=${PUNKTFUNK_INSTALL_CLIPBOARD:-}       # 1/0, empty = ask (default no)
PF_GROUP=${PUNKTFUNK_INSTALL_PUNKTFUNK_GROUP:-}  # 1/0, empty = ask (default no)
LINGER=${PUNKTFUNK_INSTALL_LINGER:-}             # 1/0, empty = ask (default no)
MGMT_PORT=${PUNKTFUNK_INSTALL_MGMT_PORT:-47991}  # where the management API moves to on a conflict
START=1
DRY=${PUNKTFUNK_INSTALL_DRY_RUN:-0}
UNINSTALL=0

usage() {
    cat <<EOF
punktfunk guided host installer (preview)

usage: sh install.sh [options]
  -y, --yes             no prompts: take every default (also the behaviour without a terminal)
  --channel stable|canary   package channel (default stable; canary = latest main build)
  --gamestream | --no-gamestream   also serve stock Moonlight clients (default no — trusted LANs only)
  --clipboard | --no-clipboard     allow the shared clipboard on this host (default no)
  --punktfunk-group | --no-punktfunk-group   join the punktfunk group (virtual Steam Deck pad; default no)
  --linger | --no-linger           start the host at boot with nobody logged in (default no)
  --mgmt-port N         port to move the management API to if Sunshine/Apollo holds 47990 (default $MGMT_PORT)
  --no-start            install and configure, but don't enable the services
  --uninstall           stop the services and remove the packages + repo (config stays: $DOCS/uninstall)
  --dry-run             print every command it would run, change nothing
  -h, --help            this text

Every option has an environment twin for scripted installs: PUNKTFUNK_INSTALL_YES=1,
PUNKTFUNK_INSTALL_CHANNEL, PUNKTFUNK_INSTALL_GAMESTREAM, PUNKTFUNK_INSTALL_CLIPBOARD,
PUNKTFUNK_INSTALL_PUNKTFUNK_GROUP, PUNKTFUNK_INSTALL_LINGER, PUNKTFUNK_INSTALL_MGMT_PORT (1/0 for the flags).
Docs: $DOCS/install
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        -y|--yes) YES=1 ;;
        --channel) shift; CHANNEL=${1:-} ;;
        --channel=*) CHANNEL=${1#*=} ;;
        --gamestream) GAMESTREAM=1 ;;      --no-gamestream) GAMESTREAM=0 ;;
        --clipboard) CLIPBOARD=1 ;;        --no-clipboard) CLIPBOARD=0 ;;
        --punktfunk-group) PF_GROUP=1 ;;   --no-punktfunk-group) PF_GROUP=0 ;;
        --linger) LINGER=1 ;;              --no-linger) LINGER=0 ;;
        --mgmt-port) shift; MGMT_PORT=${1:-} ;;
        --mgmt-port=*) MGMT_PORT=${1#*=} ;;
        --no-start) START=0 ;;
        --uninstall) UNINSTALL=1 ;;
        --dry-run) DRY=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done
case "$CHANNEL" in stable|canary) ;; *) echo "--channel must be stable or canary" >&2; exit 2 ;; esac
case "$MGMT_PORT" in ''|*[!0-9]*) echo "--mgmt-port must be a number" >&2; exit 2 ;; esac

# ---------------------------------------------------------------------------- plumbing
say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m  !!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m  xx\033[0m %s\n' "$*" >&2; exit 1; }

# Prompts read from the terminal, not stdin — stdin is the script itself under `curl | sh`.
# No terminal (CI, cron, a pipe) behaves like --yes.
# Probe by opening it: in a container or under a service /dev/tty is a node that exists but
# can't be opened (ENXIO — no controlling terminal), so -r/-w would say yes and the redirect fail.
TTY=/dev/tty
(exec 3<>/dev/tty) 2>/dev/null || { TTY=; YES=1; }

# ask "question" default(y|n) → 0 = yes, 1 = no
ask() {
    if [ "$YES" = 1 ]; then [ "$2" = y ]; return; fi
    if [ "$2" = y ]; then hint="[Y/n]"; else hint="[y/N]"; fi
    printf '\033[1m?\033[0m %s %s ' "$1" "$hint" > "$TTY"
    read -r ans < "$TTY" || ans=
    case "${ans:-$2}" in y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
}

# run "shell snippet": print it, run it (-e), and give the package manager the terminal so its own
# confirmation prompt works; under --yes the snippet is made non-interactive first.
run() {
    cmd=$1
    if [ "$YES" = 1 ]; then
        cmd=$(printf '%s' "$cmd" | sed \
            -e 's/^sudo apt install /sudo apt install -y /' \
            -e 's/^sudo dnf install /sudo dnf install -y /' \
            -e 's/^sudo pacman -Syu /sudo pacman -Syu --noconfirm /' \
            -e 's/^sudo apt purge /sudo apt purge -y /' \
            -e 's/^sudo dnf remove /sudo dnf remove -y /' \
            -e 's/^sudo pacman -Rns /sudo pacman -Rns --noconfirm /')
    fi
    printf '  + %s\n' "$cmd"
    [ "$DRY" = 1 ] && return 0
    if [ -n "$TTY" ]; then sh -ec "$cmd" < "$TTY"; else sh -ec "$cmd" < /dev/null; fi \
        || die "that step failed — fix it and re-run (the script is safe to repeat), or follow the page by hand: $DOCS_PAGE"
}

# Running as root without sudo (a minimal Debian container): a shim so the verbatim
# `sudo …` lines from platforms.json still work.
if [ "$(id -u)" = 0 ] && ! command -v sudo >/dev/null 2>&1; then
    SHIM=$(mktemp -d) && printf '#!/bin/sh\nexec "$@"\n' > "$SHIM/sudo" && chmod +x "$SHIM/sudo" && PATH="$SHIM:$PATH"
fi

HOST_ENV=${XDG_CONFIG_HOME:-$HOME/.config}/punktfunk/host.env
# set_env KEY VALUE — replace or append one line in host.env (created on first use).
set_env() {
    if [ "$DRY" = 1 ]; then ok "would set $1=$2 in $HOST_ENV"; return 0; fi
    mkdir -p "$(dirname "$HOST_ENV")"
    touch "$HOST_ENV"
    if grep -q "^$1=" "$HOST_ENV"; then
        sed -i "s|^$1=.*|$1=$2|" "$HOST_ENV"
    else
        printf '%s=%s\n' "$1" "$2" >> "$HOST_ENV"
    fi
    ok "$1=$2 → $HOST_ENV"
}

# ---------------------------------------------------------------------------- 0. preflight
cat <<EOF

  punktfunk guided host installer — PREVIEW
  The per-distro pages stay the documented path; this automates them. Docs: $DOCS/install
  Re-running is safe. Ctrl-C stops between steps.

EOF

[ "$(uname -s)" = Linux ] || [ -n "${PUNKTFUNK_INSTALL_OS_RELEASE:-}" ] || die "this installer is for Linux hosts — Windows: $DOCS/windows-host"
[ -n "${SUDO_USER:-}" ] && [ "$(id -u)" = 0 ] && die "run this as your normal user, not under sudo — it calls sudo itself where needed, and the host runs as you (host.env, the services)"
command -v curl >/dev/null 2>&1 || die "curl is required (install it with your package manager first)"
OS_RELEASE=${PUNKTFUNK_INSTALL_OS_RELEASE:-/etc/os-release}   # override for testing the detection
[ -r "$OS_RELEASE" ] || die "no /etc/os-release — can't tell which distro this is: $DOCS/install"
. "$OS_RELEASE"
ID=${ID:-}; ID_LIKE=${ID_LIKE:-}; VERSION_ID=${VERSION_ID:-}; PRETTY=${PRETTY_NAME:-$ID}
like() { case " $ID $ID_LIKE " in *" $1 "*) return 0 ;; esac; return 1; }

FAMILY=
DOCS_PAGE=$DOCS/install
if [ "$ID" = nixos ]; then
    die "NixOS: add the flake input and enable the module instead — $DOCS/nixos"
elif [ "$ID" = steamos ]; then
    die "SteamOS host: the on-device installer builds against the running OS — $DOCS/steamos-host"
elif command -v rpm-ostree >/dev/null 2>&1 || command -v bootc >/dev/null 2>&1 || [ "$ID" = bazzite ]; then
    FAMILY=sysext;  DOCS_PAGE=$DOCS/bazzite
elif like debian || like ubuntu; then
    FAMILY=apt;     DOCS_PAGE=$DOCS/debian
    [ "$ID" = ubuntu ] && DOCS_PAGE=$DOCS/ubuntu
elif like fedora; then
    FAMILY=dnf;     DOCS_PAGE=$DOCS/fedora
elif like arch; then
    FAMILY=pacman;  DOCS_PAGE=$DOCS/arch
    # Omarchy is Arch underneath — same repo, same packages, same commands — so it is a FLAVOUR of
    # the pacman family, not a family of its own. What differs is everything after the install:
    # ufw is on by default, autostart is a user unit bound to graphical-session.target, the console
    # belongs in their app menu, and updates go through `omarchy update`. `punktfunk-omarchy setup`
    # is the one command that does all of it; the guide is its own page.
    [ "$ID" = omarchy ] && DOCS_PAGE=$DOCS/omarchy
else
    die "no package repo for '$PRETTY' yet — $DOCS/build-from-source"
fi
say "Detected $PRETTY → $FAMILY (guide: $DOCS_PAGE)"

# ---------------------------------------------------------------------------- --uninstall
# The reverse of step 1 + step 6, as $DOCS/uninstall spells it out per family: user units off first
# (package removal can't see the enable symlinks in $HOME), then only the punktfunk packages that
# are actually installed, then the repo. Config, groups and firewall rules stay — the page lists them.
if [ "$UNINSTALL" = 1 ]; then
    say "Uninstalling the host ($DOCS/uninstall)"
    run 'systemctl --user disable --now punktfunk-host punktfunk-web punktfunk-scripting 2>/dev/null || true'
    case "$FAMILY" in
        apt)
            pkgs=$(dpkg-query -W -f='${Package} ${db:Status-Status}\n' 'punktfunk*' 2>/dev/null | awk '$2=="installed"{printf "%s ", $1}')
            [ -n "$pkgs" ] && run "sudo apt purge $pkgs"
            run 'sudo rm -f /etc/apt/sources.list.d/punktfunk.list /etc/apt/keyrings/punktfunk.asc'
            run 'sudo apt update'
            ;;
        dnf)
            pkgs=$(rpm -qa --qf '%{NAME} ' 'punktfunk*' 2>/dev/null)
            [ -n "$pkgs" ] && run "sudo dnf remove $pkgs"
            run 'sudo rm -f /etc/yum.repos.d/punktfunk.repo'
            ;;
        pacman)
            pkgs=$(pacman -Qq 2>/dev/null | grep '^punktfunk' | tr '\n' ' ')
            [ -n "$pkgs" ] && run "sudo pacman -Rns $pkgs"
            run "sudo sed -i '/^\\[punktfunk\\(-canary\\)\\{0,1\\}\\]\$/,/^Server = /d' /etc/pacman.conf"
            ;;
        sysext)
            run 'sudo punktfunk-sysext remove'
            ;;
    esac
    cat <<EOF

  Removed. Left on purpose: ~/.config/punktfunk (identity, pairings, host.env, plugins — a reinstall
  picks them up), the punktfunk / punktfunk-update groups, and any firewall rules you opened.
  The one-command cleanups for each are on $DOCS/uninstall#linux-hosts
EOF
    exit 0
fi

# Version floors the package can't express: below these the install succeeds and nothing can stream.
major=${VERSION_ID%%.*}
case "$ID" in
    debian) [ "${major:-0}" -ge 13 ] 2>/dev/null || die "Debian $VERSION_ID is below the glibc floor — Debian 13+ or build from source: $DOCS/build-from-source" ;;
    ubuntu) case "$VERSION_ID" in 2[0-5].*) warn "Ubuntu $VERSION_ID installs the package but cannot host — its desktop is too old to create a virtual display ($DOCS/requirements#the-floor-for-a-working-host). Use 26.04+."
                ask "Continue anyway?" n || exit 1 ;; esac ;;
    linuxmint) case "$VERSION_ID" in 2[0-2]*) warn "Linux Mint $VERSION_ID (Ubuntu 24.04 base) installs the package but cannot host — $DOCS/requirements#cinnamon-linux-mint-and-lmde. LMDE 7 and Mint 23 can."
                ask "Continue anyway?" n || exit 1 ;; esac ;;
esac
RPM_GROUP=
if [ "$FAMILY" = dnf ]; then
    case "$major" in
        44) RPM_GROUP=fedora-44 ;;
        43) RPM_GROUP=bazzite ;;      # a plain Fedora 43 build of the same package
        *)  die "no RPM group for Fedora $VERSION_ID yet — $DOCS/build-from-source" ;;
    esac
fi

# ---------------------------------------------------------------------------- 1. install
# The snippets below are data/platforms.json's install lines, verbatim (stable channel); canary
# and the Fedora group are edited in. check-docs-drift.sh gate 6 keeps them identical.
#
# The host, the console and the plugin runner are three separate packages on every family, so "is
# the host there?" is the wrong question to skip the install on. A box that has the host but no
# console — installed by hand, from an older docs line, or by a package manager told to drop weak
# deps (dnf `install_weak_deps=False`, APT::Install-Recommends "0") — would never get one however
# often this ran, and the console is where you pair, approve a device and change every setting.
# Ask per binary instead: each family's line below names all three, and installing one that is
# already there is a no-op.
have() { command -v "$1" >/dev/null 2>&1; }
MISSING=
have punktfunk-host       || MISSING="$MISSING host"
have punktfunk-web-server || MISSING="$MISSING web-console"
have punktfunk-scripting  || MISSING="$MISSING plugin-runner"
if [ -z "$MISSING" ]; then
    say "host, web console and plugin runner are already installed ($(punktfunk-host --version 2>/dev/null | head -1)) — skipping the install, continuing with setup"
else
    say "Installing:$MISSING ($CHANNEL channel)"
    case "$FAMILY" in
        apt)
            repo_line='echo "deb [signed-by=/etc/apt/keyrings/punktfunk.asc] https://git.unom.io/api/packages/unom/debian stable main" | sudo tee /etc/apt/sources.list.d/punktfunk.list'
            [ "$CHANNEL" = canary ] && repo_line=$(printf '%s' "$repo_line" | sed 's/ stable main/ canary main/')
            run 'sudo install -d -m 0755 /etc/apt/keyrings'
            run 'curl -fsSL https://git.unom.io/api/packages/unom/debian/repository.key | sudo tee /etc/apt/keyrings/punktfunk.asc >/dev/null'
            run "$repo_line"
            run 'sudo apt update'
            run 'sudo apt install punktfunk-host punktfunk-web punktfunk-scripting'
            ;;
        pacman)
            repo_line=$(cat <<'LINE'
grep -q '^\[punktfunk\]' /etc/pacman.conf || printf '\n[punktfunk]\nServer = https://git.unom.io/api/packages/unom/arch/$repo/$arch\n' | sudo tee -a /etc/pacman.conf >/dev/null
LINE
)
            # canary: both the grep guard (escaped brackets) and the printf body name the repo
            [ "$CHANNEL" = canary ] && repo_line=$(printf '%s' "$repo_line" | sed -e 's/punktfunk\\\]/punktfunk-canary\\]/' -e 's/\[punktfunk\]/[punktfunk-canary]/')
            run 'curl -fsS https://git.unom.io/api/packages/unom/arch/repository.key | sudo pacman-key --add -'
            run 'sudo pacman-key --lsign-key E0CA04465C99C936E0B0C6510A317015A34DDD69'
            run "$repo_line"
            # Omarchy ships a libalpm PreTransaction hook that ABORTS any transaction whose pacman
            # invocation carries both -S and -u, to funnel system upgrades through `omarchy update`.
            # So Arch's one-liner dies there with "Woah partner..." and installs nothing (measured
            # on 4.0.1). `-Sy` refreshes without a sysupgrade and is not blocked; `-S` then installs
            # exactly the three packages. On plain Arch the full `-Syu` stays right — a partial
            # upgrade against a ROLLING repo is the thing that breaks those boxes, and Omarchy's
            # frozen snapshot mirror is precisely why it does not break here.
            if [ "$ID" = omarchy ]; then
                run 'sudo pacman -Sy'
                run 'sudo pacman -S punktfunk-host punktfunk-web punktfunk-scripting'
            else
                run 'sudo pacman -Syu punktfunk-host punktfunk-web punktfunk-scripting'
            fi
            ;;
        dnf)
            group=$RPM_GROUP
            [ "$CHANNEL" = canary ] && group="$group-canary"
            run "$(cat <<'CMD'
sudo tee /etc/yum.repos.d/punktfunk.repo >/dev/null <<'REPO'
[punktfunk]
name=punktfunk
# fedora-44 on Fedora 44; bazzite on Fedora 43 (a plain Fedora 43 build of the same package)
baseurl=https://git.unom.io/api/packages/unom/rpm/fedora-44
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=https://git.unom.io/api/packages/unom/rpm/repository.key
       https://git.unom.io/api/packages/unom/generic/punktfunk-keys/1/RPM-GPG-KEY-punktfunk
REPO
CMD
)"
            [ "$group" = fedora-44 ] || run "sudo sed -i 's|/rpm/fedora-44|/rpm/$group|' /etc/yum.repos.d/punktfunk.repo"
            run 'sudo dnf install punktfunk punktfunk-web punktfunk-scripting'
            ;;
        sysext)
            install_line='sudo bash punktfunk-sysext.sh install'
            [ "$CHANNEL" = canary ] && install_line="$install_line --channel canary"
            run 'curl -fsSLO https://git.unom.io/unom/punktfunk/raw/branch/main/packaging/bazzite/punktfunk-sysext.sh'
            run "$install_line"
            ;;
    esac
    hash -r 2>/dev/null || true
    if [ "$DRY" != 1 ]; then
        have punktfunk-host || die "the install finished but punktfunk-host isn't on PATH — open a new terminal and re-run, or see $DOCS_PAGE"
        ok "punktfunk-host $(punktfunk-host --version 2>/dev/null | head -1) installed"
        # Not fatal — the host still streams — but say it out loud here rather than let step 7
        # hand out a console URL for something that is not on the box.
        if have punktfunk-web-server; then ok "the web console (punktfunk-web) is installed"
        else warn "the web console (punktfunk-web) did NOT get installed — pairing, approving a device and every setting live there. Install it by hand: $DOCS_PAGE"; fi
    fi
fi

# ------------------------------------------------------------------- 1b. Omarchy hand-off
# Everything from here to step 6 is the generic Linux wiring: join a group, open the firewall wide,
# enable a user unit. On Omarchy each of those has a better local answer — LAN-scoped tagged ufw
# rules, a drop-in that ties the host to the session uwsm actually starts, the console as an entry
# in their app menu, toasts through their notifier — and `punktfunk-omarchy setup` is the one
# command that does all of them AND knows how to reverse itself. So offer it instead of doing a
# second, weaker version of the same work. Declining just continues generically; nothing is lost.
if [ "$ID" = omarchy ]; then
    say "Omarchy"
    if have punktfunk-omarchy || [ "$DRY" = 1 ]; then
        if ask "Finish with the Omarchy integration (ufw scoped to your LAN, autostart with the session, console in the app menu, optional toasts)?" y; then
            run 'punktfunk-omarchy setup'
            [ "$DRY" != 1 ] && exit 0
        else
            echo "     Run it later with: punktfunk-omarchy setup   ($DOCS/omarchy)"
        fi
    else
        warn "punktfunk-omarchy is not on PATH — the host package should ship it; see $DOCS/omarchy"
    fi
fi

# ---------------------------------------------------------------------------- 2. another host?
# detect-conflicts exits 1 only for a Sunshine-family host that runs or autostarts; dormant
# leftovers print and exit 0. Native-only, the single port both want is the management API's.
say "Checking for Sunshine / Apollo / Vibeshine"
CONFLICT=0
if [ "$DRY" = 1 ] && ! command -v punktfunk-host >/dev/null 2>&1; then
    printf '  + %s\n' 'punktfunk-host detect-conflicts'
elif report=$(punktfunk-host detect-conflicts 2>&1); then
    ok "${report:-No conflicting game-streaming host detected.}"
else
    CONFLICT=1
    printf '%s\n' "$report" | sed 's/^/     /'
    warn "another streaming host is active on this box — both want TCP 47990 (its web UI, punktfunk's management API)"
    if ask "Keep both for now and move punktfunk's management API to port $MGMT_PORT? (No = you stop the other host yourself)" y; then
        set_env PUNKTFUNK_MGMT_BIND "0.0.0.0:$MGMT_PORT"
        echo "     Clients learn the port from discovery; the console and plugins read it from mgmt-endpoint. Details: $DOCS/switching-from-sunshine"
    else
        MGMT_PORT=
        echo "     Stop it before you start punktfunk (e.g. sudo systemctl disable --now sunshine) — $DOCS/switching-from-sunshine"
    fi
fi
[ "$CONFLICT" = 0 ] && MGMT_PORT=

# ---------------------------------------------------------------------------- 3. groups
say "Controller access"
if id -nG "$USER" 2>/dev/null | tr ' ' '\n' | grep -qx input; then
    ok "already in the input group"
elif command -v ujust >/dev/null 2>&1; then   # Bazzite: the input group is recipe-managed, usermod is the wrong tool
    run 'ujust add-user-to-input-group'; RELOGIN=1
elif ! getent group input >/dev/null 2>&1; then
    warn "no 'input' group on this system — virtual gamepads need /dev/uinput access; see $DOCS_PAGE"
else
    run 'sudo usermod -aG input "$USER"'; RELOGIN=1
fi
if [ -z "$PF_GROUP" ]; then
    ask "Also join the punktfunk group? It enables the virtual Steam Deck controller (paddles, trackpads, gyro) by granting usbip attach — only on a machine you trust ($DOCS/gamescope#nobara-and-other-autologin-display-managers)" n && PF_GROUP=1 || PF_GROUP=0
fi
if [ "$PF_GROUP" = 1 ]; then
    if id -nG "$USER" 2>/dev/null | tr ' ' '\n' | grep -qx punktfunk; then ok "already in the punktfunk group"
    else run 'sudo usermod -aG punktfunk "$USER"'; RELOGIN=1; fi
fi

# ---------------------------------------------------------------------------- 4. options
say "Options (host.env — everything here is off by default and reversible)"
if [ -z "$GAMESTREAM" ]; then
    ask "Also serve stock Moonlight clients (GameStream compat)? Its pairing runs over plain HTTP — trusted LANs only; Punktfunk's own apps don't need it" n && GAMESTREAM=1 || GAMESTREAM=0
fi
if [ "$GAMESTREAM" = 1 ]; then
    [ "$CONFLICT" = 1 ] && warn "with another GameStream host running, only one can bind the Moonlight ports — stop the other first or skip this"
    set_env PUNKTFUNK_GAMESTREAM 1
fi
if [ -z "$CLIPBOARD" ]; then
    ask "Allow the shared clipboard on this host (each client still opts in per host)?" n && CLIPBOARD=1 || CLIPBOARD=0
fi
[ "$CLIPBOARD" = 1 ] && set_env PUNKTFUNK_CLIPBOARD on

# ---------------------------------------------------------------------------- 5. firewall
# Packages never open ports; they install firewalld services / ufw profiles by these names.
say "Firewall"
if command -v firewall-cmd >/dev/null 2>&1 && systemctl is-active --quiet firewalld 2>/dev/null; then
    svcs="--add-service=punktfunk-native --add-service=punktfunk-web"
    [ "$GAMESTREAM" = 1 ] && svcs="$svcs --add-service=punktfunk-gamestream"
    [ -n "$MGMT_PORT" ] && svcs="$svcs --add-port=$MGMT_PORT/tcp"
    run 'sudo firewall-cmd --reload'
    run "sudo firewall-cmd --permanent $svcs"
    run 'sudo firewall-cmd --reload'
elif command -v ufw >/dev/null 2>&1 && grep -qs '^ENABLED=yes' /etc/ufw/ufw.conf; then
    run 'sudo ufw allow punktfunk-native'
    run 'sudo ufw allow punktfunk-web'
    [ "$GAMESTREAM" = 1 ] && run 'sudo ufw allow punktfunk-gamestream'
    [ -n "$MGMT_PORT" ] && run "sudo ufw allow $MGMT_PORT/tcp"
else
    ok "no active firewall found — nothing to open ($DOCS/ports if you add one later)"
fi

# ---------------------------------------------------------------------------- 6. start
if [ "$START" = 1 ]; then
    say "Starting the host and the web console"
    if ! systemctl --user show-environment >/dev/null 2>&1; then
        warn "no user systemd session here (ssh without a login session?) — run this from a terminal in your desktop session:"
        echo "     systemctl --user enable --now punktfunk-host punktfunk-web"
        START=0
    else
        systemctl --user daemon-reload 2>/dev/null
        units="punktfunk-host"
        if systemctl --user list-unit-files punktfunk-web.service 2>/dev/null | grep -q '^punktfunk-web.service'; then
            units="$units punktfunk-web"
        else
            warn "no punktfunk-web.service on this box — the console is not installed, so nothing will answer on 47992 ($DOCS_PAGE)"
        fi
        # The plugin runner fills the game library; apt/dnf/sysext start it themselves, Arch doesn't.
        if systemctl --user list-unit-files punktfunk-scripting.service 2>/dev/null | grep -q disabled; then units="$units punktfunk-scripting"; fi
        run "systemctl --user enable --now $units"
        if [ -z "$LINGER" ]; then
            ask "Start the host at boot even with nobody logged in (headless box)?" n && LINGER=1 || LINGER=0
        fi
        [ "$LINGER" = 1 ] && run 'sudo loginctl enable-linger "$USER"'
    fi
fi

# ---------------------------------------------------------------------------- 7. verify + next
say "Checking"
if [ "$START" = 1 ] && [ "$DRY" != 1 ]; then
    sleep 2
    if systemctl --user is-active --quiet punktfunk-host; then ok "punktfunk-host is running"
    else warn "punktfunk-host is not active — journalctl --user -u punktfunk-host -e ($DOCS/troubleshooting#the-linux-host-service-wont-start)"; fi
    if command -v ss >/dev/null 2>&1 && ss -lun 2>/dev/null | grep -q ':9777 '; then ok "listening on UDP 9777 (punktfunk/1)"
    else warn "nothing on UDP 9777 yet — give it a second, then: journalctl --user -u punktfunk-host -e"; fi
fi
# GPU drivers are the docs pages' job (one step, per distro) — but the silent failures worth
# calling out, because the install succeeded and streaming won't: an NVIDIA card whose kernel
# module didn't load (Secure Boot blocks the unenrolled key — nvidia-smi can't talk to it), or no
# driver at all; and Fedora + NVIDIA with Fedora's own ffmpeg, which has no NVENC (the RPM only
# Recommends RPM Fusion's build).
if grep -qs 0x10de /sys/bus/pci/devices/*/vendor 2>/dev/null; then
    if ! command -v nvidia-smi >/dev/null 2>&1; then
        warn "NVIDIA GPU without the NVIDIA driver — nothing can encode until it's installed: step 1 of $DOCS_PAGE"
    elif ! nvidia-smi >/dev/null 2>&1; then
        warn "NVIDIA GPU, but nvidia-smi can't talk to the driver — the kernel module didn't load (Secure Boot? run: mokutil --sb-state): $DOCS/troubleshooting#nvidia-smi-says-it-cant-communicate-with-the-driver"
    fi
    if [ "$FAMILY" = dnf ] && ! rpm -q ffmpeg-libs >/dev/null 2>&1; then
        warn "NVIDIA GPU, but RPM Fusion's ffmpeg-libs isn't installed — NVENC won't work until it is: step 1 of $DOCS_PAGE"
    fi
fi
ip=$(hostname -I 2>/dev/null | awk '{print $1}')
[ -n "$ip" ] || ip=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="src") print $(i+1); exit}')
# Step 1 is the console, so it must not be printed as fact when the console isn't installed —
# that is what sent a Fedora user looking for a page nothing was serving. (--dry-run installs
# nothing by definition, so it shows the normal text.)
if have punktfunk-web-server || [ "$DRY" = 1 ]; then
    step1="1. Open the web console:  https://${ip:-<host-ip>}:47992  (the certificate is the host's own — continue past the warning)
     password:  sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password"
else
    step1="1. Install the web console — it is NOT on this box, and pairing, approving a device and
     every setting live there. The install line for your distro is on $DOCS_PAGE"
fi
cat <<EOF

  Done. Next:
  $step1
  2. Install a client on the device you stream to ($DOCS/install-client), connect, and click
     Approve in the console — or Pair a device for a PIN ($DOCS/pairing).
  3. Stream. Ctrl+Alt+Shift+Q hands mouse and keyboard back on desktop clients.
EOF
[ "${RELOGIN:-0}" = 1 ] && echo "  Group changes apply after you log out and back in (controllers won't work until then)."
[ "$CONFLICT" = 1 ] && echo "  Running next to Sunshine/Apollo: $DOCS/switching-from-sunshine"
echo "  Stuck? $DOCS/troubleshooting · this installer is a preview — the full guide is $DOCS_PAGE"
echo
