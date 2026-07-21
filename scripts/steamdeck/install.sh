#!/usr/bin/env bash
# punktfunk — Steam Deck HOST installer (stream FROM the Deck to other devices).
#
# SteamOS is an immutable, read-only Arch base, so the host can't be a system package and a
# prebuilt binary would break on an OS library bump. Instead we build the host natively inside a
# Debian-trixie distrobox (ABI-matched to SteamOS's FFmpeg/glibc) — the binary then runs natively
# on SteamOS — and wire it up as proper systemd USER services. A rebuild always matches the
# running OS. AMD encode uses VAAPI; NVIDIA uses NVENC (auto-detected).
#
# Run it on the Deck (Desktop Mode "Konsole", or over ssh). Idempotent — safe to re-run to update
# config or pick up new options. To rebuild after pulling new source, use update.sh.
#
#   bash scripts/steamdeck/install.sh                 # PIN pairing required; Moonlight compat ON
#   bash scripts/steamdeck/install.sh --no-gamestream # SECURE native-only (no Moonlight/#5/#9 surface)
#   bash scripts/steamdeck/install.sh --open          # trusted LAN: accept unpaired clients (TOFU)
#   bash scripts/steamdeck/install.sh --no-web        # skip the management web console
#   PUNKTFUNK_SRC=~/src/punktfunk bash scripts/steamdeck/install.sh   # source elsewhere
#
set -euo pipefail

log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m  !!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- options ---------------------------------------------------------------
SRC="${PUNKTFUNK_SRC:-$HOME/punktfunk}"
BOX="${PUNKTFUNK_BOX:-pf2}"
BOX_IMAGE="${PUNKTFUNK_BOX_IMAGE:-docker.io/library/debian:trixie}"
MGMT_PORT="${PUNKTFUNK_MGMT_PORT:-47990}"
WEB_PORT="${PUNKTFUNK_WEB_PORT:-47992}"
OPEN=0
WITH_WEB=1
GAMESTREAM=1 # Moonlight/GameStream compat on by default; --no-gamestream for a secure native-only host
for arg in "$@"; do
    case "$arg" in
        --open) OPEN=1 ;;
        --no-web) WITH_WEB=0 ;;
        --no-gamestream) GAMESTREAM=0 ;;
        --src=*) SRC="${arg#--src=}" ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) die "unknown option: $arg (try --help)" ;;
    esac
done
TARGET_DIR="$SRC/target-steamos"
BIN="$TARGET_DIR/release/punktfunk-host"
CONFIG="$HOME/.config/punktfunk"
UNITS="$HOME/.config/systemd/user"
XRD="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
# Set when this run does something that only a fresh login picks up (input-group add, first-time
# KWin .desktop grant). Drives the loud "reboot before streaming" note in the summary.
NEED_RELOGIN=0

# --- 0. preflight ----------------------------------------------------------
log "Preflight"
[ -f /etc/os-release ] && . /etc/os-release || true
case "${ID:-}${ID_LIKE:-}" in
    *steamos*|*arch*) ok "SteamOS / Arch base detected (${PRETTY_NAME:-unknown})" ;;
    *) warn "This installer targets SteamOS; '${PRETTY_NAME:-unknown}' may differ — on a normal distro use the apt/rpm packages instead." ;;
esac
[ -d "$SRC/crates/punktfunk-host" ] || die "no punktfunk source at $SRC. Clone or rsync it there first, or pass --src=DIR (see scripts/steamdeck/README.md)."
ok "source: $SRC"
if ! have distrobox; then
    die "distrobox not found. Install it once (no root needed):
       curl -sfL https://raw.githubusercontent.com/89luca89/distrobox/main/install | sh -s -- --prefix ~/.local
     then re-run this script (ensure ~/.local/bin is on PATH)."
fi
DISTROBOX="$(command -v distrobox)"   # baked into the web unit (may be /usr/bin or ~/.local/bin)
ok "distrobox: $DISTROBOX"

# --- acquire sudo up front (before the ~15-min build) ----------------------
# Steps 4-5 (UDP buffers, gamepad udev rule, vhci-hcd, input group, linger) need root. Prompt NOW,
# not after the build — so you authorize once and walk away, and a non-interactive run fails LOUDLY
# here instead of silently skipping the tuning at the very end. A stock SteamOS 'deck' has no
# password, so sudo can't work until you set one.
SUDO_OK=0
if sudo -n true 2>/dev/null; then
    SUDO_OK=1
elif [ -t 0 ]; then
    warn "sudo is needed once (UDP buffers, gamepad udev rule, vhci-hcd, input group, linger):"
    if sudo -v; then
        SUDO_OK=1
        # keep the sudo timestamp warm across the long build so steps 4-5 don't re-prompt / expire
        ( while sudo -n -v 2>/dev/null; do sleep 50; done ) &
        _pf_sudo_keepalive=$!
        trap '[ -n "${_pf_sudo_keepalive:-}" ] && kill "$_pf_sudo_keepalive" 2>/dev/null || true' EXIT
    fi
fi
if [ "$SUDO_OK" != 1 ]; then
    if [ -t 0 ]; then
        warn "No sudo — a stock SteamOS 'deck' account has no password. Set one and re-run:  passwd"
    else
        warn "No TTY for the sudo prompt (non-interactive run) — system tuning + linger will be SKIPPED."
        warn "Run in Konsole or an interactive 'ssh -t' session (or pre-authorize sudo) to enable them."
    fi
fi

# --- 1. build container + toolchain ---------------------------------------
log "Build container '$BOX' ($BOX_IMAGE)"
if distrobox list 2>/dev/null | awk -F'|' '{gsub(/ /,"",$2); print $2}' | grep -qx "$BOX"; then
    ok "container '$BOX' exists"
else
    log "creating '$BOX' (first time — pulls the image)…"
    distrobox create --yes --name "$BOX" --image "$BOX_IMAGE" --home "$HOME"
    ok "created '$BOX'"
fi

log "Provisioning build dependencies in '$BOX' (idempotent; apt + rustup + bun)"
# One non-interactive provisioning pass. APT deps mirror the Linux host build (FFmpeg/PipeWire/
# DRM/EGL/VAAPI dev libs); rustup + bun are per-user under the shared $HOME.
distrobox enter "$BOX" -- bash -lc '
set -e
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
sudo apt-get install -y -qq --no-install-recommends \
    build-essential pkg-config clang cmake curl git ca-certificates \
    libavcodec-dev libavformat-dev libavutil-dev libavfilter-dev libswscale-dev libavdevice-dev \
    libpipewire-0.3-dev libspa-0.2-dev \
    libgbm-dev libegl-dev libgl-dev libdrm-dev libva-dev \
    libxkbcommon-dev libudev-dev libssl-dev libopus-dev libsdl2-dev \
    nodejs >/dev/null
command -v rustc >/dev/null 2>&1 || command -v ~/.cargo/bin/rustc >/dev/null 2>&1 || \
    curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path >/dev/null
# bun builds AND runs the web console now (the Nitro `bun` preset + our Bun.serve TLS entry —
# bun-native output, so the old srvx mis-resolution that forced node no longer applies).
command -v bun >/dev/null 2>&1 || command -v ~/.bun/bin/bun >/dev/null 2>&1 || \
    curl -fsSL https://bun.sh/install | bash >/dev/null
'
ok "build deps ready"

# --- 2. build host (+ web) -------------------------------------------------
log "Building punktfunk-host (release) — first build is slow (~10-15 min)"
# vulkan-encode matches the packaged builds (deb/arch): the raw Vulkan Video HEVC/AV1 backend
# (real RFI loss recovery). Pure-Rust ash — no extra system dep. A featureless hand build would
# silently fall back to libav VAAPI.
distrobox enter "$BOX" -- bash -lc "
set -e
export PATH=\$HOME/.cargo/bin:\$PATH CARGO_TARGET_DIR='$TARGET_DIR'
cd '$SRC' && cargo build -r -p punktfunk-host --features punktfunk-host/vulkan-encode
"
[ -x "$BIN" ] || die "build did not produce $BIN"
ok "host binary: $BIN"

if [ "$WITH_WEB" = 1 ]; then
    log "Building the management web console (bun)"
    distrobox enter "$BOX" -- bash -lc "
set -e
export PATH=\$HOME/.bun/bin:\$PATH
cd '$SRC/web' && bun install --frozen-lockfile && bun run build
"
    [ -f "$SRC/web/.output/server/index.mjs" ] || die "web build did not produce web/.output/server/index.mjs"
    ok "web console built"
fi

# --- 3. config -------------------------------------------------------------
log "Configuration ($CONFIG)"
mkdir -p "$CONFIG"
if [ ! -f "$CONFIG/host.env" ]; then
    cat > "$CONFIG/host.env" <<'EOF'
# punktfunk Steam Deck host config (sourced by the punktfunk-host user service).
# Auto encoder: Vulkan Video (or VAAPI fallback) on the Deck's AMD GPU, NVENC on NVIDIA.
PUNKTFUNK_ENCODER=auto
# Van Gogh (LCD/OLED Deck) RADV still gates VK_KHR_video_encode_* behind this perftest flag;
# without it the Vulkan backend can't open and sessions fall back to libav VAAPI. Harmless on
# GPUs where encode is exposed by default.
RADV_PERFTEST=video_encode
# The host auto-detects the live session (Game Mode gamescope / Desktop KDE) per connect.
# Override the compositor only if detection misbehaves: PUNKTFUNK_COMPOSITOR=gamescope
EOF
    ok "wrote host.env"
else
    ok "host.env exists (left as-is)"
fi

# KWin authorization for Desktop-Mode streaming (and mid-stream Game↔Desktop switches): KWin
# resolves a connecting client's /proc/<pid>/exe against a .desktop `Exec=` and only then grants
# the restricted Wayland globals it lists (see packaging/linux/io.unom.Punktfunk.Host.desktop).
# Exec must therefore be THIS install's binary path, not the packaged /usr/bin one. KWin reads
# grants at session start — after first install, restart the Desktop session (Game Mode and back).
DESKTOP_DST="$HOME/.local/share/applications/io.unom.Punktfunk.Host.desktop"
# First-time install of the grant: KWin only reads it at session start, so a fresh login is required
# before Desktop-mode capture works. A re-run that just rewrites it needs no relogin.
[ -f "$DESKTOP_DST" ] || NEED_RELOGIN=1
mkdir -p "$HOME/.local/share/applications"
sed "s|^Exec=.*|Exec=$BIN|" "$SRC/packaging/linux/io.unom.Punktfunk.Host.desktop" > "$DESKTOP_DST"
ok "KWin desktop-capture authorization (io.unom.Punktfunk.Host.desktop → $BIN)"

# KDE Desktop-mode INPUT: a normal Plasma login lacks the RemoteDesktop portal grant the host's libei
# input path needs, so it would pop an "Allow remote control?" dialog a headless host can't answer.
# Seed it once (per-user, no root) — mirrors packaging/bazzite/kde-desktop-setup.sh. Game Mode
# (gamescope) needs none of this; the .desktop above already grants org_kde_kwin_fake_input.
GRANT_SRC="$SRC/scripts/headless/kde-authorized"
GRANT_DST="$HOME/.local/share/flatpak/db/kde-authorized"
if [ -s "$GRANT_DST" ]; then
    ok "KDE RemoteDesktop grant already present"
elif [ -s "$GRANT_SRC" ]; then
    mkdir -p "$(dirname "$GRANT_DST")"
    install -m644 "$GRANT_SRC" "$GRANT_DST"
    systemctl --user restart xdg-permission-store 2>/dev/null || true
    ok "seeded KDE RemoteDesktop grant (Desktop-mode input)"
fi

if [ "$WITH_WEB" = 1 ] && [ ! -f "$CONFIG/web.env" ]; then
    # Random login password + session secret for the web console, generated once.
    # `|| true` swallows the SIGPIPE `tr` takes when `head` closes the pipe (pipefail would abort).
    WEB_PW="$(LC_ALL=C tr -dc 'a-z0-9' </dev/urandom 2>/dev/null | head -c 12 || true)"
    WEB_SECRET="$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom 2>/dev/null | head -c 32 || true)"
    cat > "$CONFIG/web.env" <<EOF
PUNKTFUNK_UI_PASSWORD=$WEB_PW
PUNKTFUNK_UI_SECRET=$WEB_SECRET
EOF
    chmod 600 "$CONFIG/web.env"
    ok "wrote web.env (generated login password)"
else
    [ "$WITH_WEB" = 1 ] && ok "web.env exists (login password unchanged)"
fi

# --- 4. system tuning (needs sudo: UDP buffers + gamepad udev rule + vhci-hcd + input group) --------
log "System tuning (UDP buffers + gamepad rules + vhci-hcd + input group)"
# sudo was acquired up front in preflight (SUDO_OK) so this never stalls behind the long build; a
# skip here (no password / no TTY) was already reported loudly there.
if [ "$SUDO_OK" = 1 ]; then
    printf 'net.core.wmem_max=33554432\nnet.core.rmem_max=33554432\n' \
        | sudo tee /etc/sysctl.d/99-punktfunk-net.conf >/dev/null
    sudo sysctl -q -p /etc/sysctl.d/99-punktfunk-net.conf >/dev/null
    ok "UDP socket buffers raised to 32 MB (persisted)"
    if [ -f "$SRC/scripts/60-punktfunk.rules" ]; then
        sudo install -m644 "$SRC/scripts/60-punktfunk.rules" /etc/udev/rules.d/60-punktfunk.rules
        sudo udevadm control --reload-rules && sudo udevadm trigger || true
        ok "installed udev rule (virtual gamepads + native Steam Deck controller)"
    fi
    # vhci-hcd: the usbip transport that makes the virtual Steam Deck pad a *real* USB device so Steam
    # Input adopts it (else it degrades to plain UHID, which Steam ignores — "no controller appears").
    # Persist the autoload AND load it now so passthrough works without waiting for a reboot.
    if [ -f "$SRC/scripts/punktfunk-modules.conf" ]; then
        sudo install -m644 "$SRC/scripts/punktfunk-modules.conf" /etc/modules-load.d/punktfunk.conf
        sudo modprobe vhci-hcd 2>/dev/null || warn "could not load vhci-hcd now (loads on next boot) — needed for the native Steam Deck pad"
        ok "vhci-hcd autoload installed (native Steam Deck controller transport)"
    fi
    if id -nG "$USER" | grep -qw input; then
        ok "already in the 'input' group"
    else
        sudo usermod -aG input "$USER"
        NEED_RELOGIN=1
        warn "added $USER to the 'input' group (applies on next login)"
    fi
else
    warn "no usable sudo — SKIPPED system tuning. Gamepad passthrough + clean streaming need root (udev"
    warn "rule, 'input' group, vhci-hcd, UDP buffers) — there is no user-space way to do these."
    warn "A stock SteamOS 'deck' account has NO password, so sudo can't work until you set one:"
    warn "  passwd            # set a sudo password once, then re-run this script"
    warn "Or apply it by hand (then reboot):"
    warn "  sudo install -m644 $SRC/scripts/60-punktfunk.rules /etc/udev/rules.d/ &&"
    warn "  sudo install -m644 $SRC/scripts/punktfunk-modules.conf /etc/modules-load.d/punktfunk.conf &&"
    warn "  sudo usermod -aG input $USER &&"
    warn "  printf 'net.core.wmem_max=33554432\\nnet.core.rmem_max=33554432\\n' | sudo tee /etc/sysctl.d/99-punktfunk-net.conf &&"
    warn "  sudo sysctl --system && sudo udevadm control --reload-rules && sudo udevadm trigger"
fi

# --- 5. systemd user services ---------------------------------------------
log "Installing systemd user services"
mkdir -p "$UNITS"
# The native punktfunk/1 plane is always on; --gamestream additionally enables the Moonlight-compat
# planes (the Deck commonly streams to Moonlight too). --no-gamestream → secure native-only (no #5/#9
# surface; native clients only).
SERVE_ARGS="serve --mgmt-bind 0.0.0.0:$MGMT_PORT"
[ "$GAMESTREAM" = 1 ] && SERVE_ARGS="$SERVE_ARGS --gamestream"
[ "$OPEN" = 1 ] && SERVE_ARGS="$SERVE_ARGS --open"
cat > "$UNITS/punktfunk-host.service" <<EOF
# Generated by scripts/steamdeck/install.sh — punktfunk Steam Deck host (native binary).
[Unit]
Description=punktfunk host (GameStream + punktfunk/1)
After=pipewire.service

[Service]
EnvironmentFile=%h/.config/punktfunk/host.env
Environment=XDG_RUNTIME_DIR=$XRD
Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=$XRD/bus
ExecStart=$BIN $SERVE_ARGS
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
EOF
ok "punktfunk-host.service ($SERVE_ARGS)"

if [ "$WITH_WEB" = 1 ]; then
    # The console is a Nitro server run by bun (Bun.serve, HTTPS — HTTP/1.1 over TLS — with the host's
    # identity cert); it lives in the build container and proxies to the host's loopback HTTPS mgmt API.
    cat > "$UNITS/punktfunk-web.service" <<EOF
# Generated by scripts/steamdeck/install.sh — punktfunk web console (bun in the '$BOX' distrobox).
[Unit]
Description=punktfunk management web console
After=punktfunk-host.service

[Service]
ExecStart=$DISTROBOX enter $BOX -- bash -lc 'cd $SRC/web; set -a; . $CONFIG/mgmt-token; . $CONFIG/web.env; set +a; export PUNKTFUNK_MGMT_URL=https://127.0.0.1:$MGMT_PORT PORT=$WEB_PORT HOST=0.0.0.0 NITRO_PORT=$WEB_PORT NITRO_HOST=0.0.0.0 PUNKTFUNK_UI_TLS_CERT=$CONFIG/cert.pem PUNKTFUNK_UI_TLS_KEY=$CONFIG/key.pem PUNKTFUNK_UI_SECURE=1; exec bun .output/server/index.mjs'
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
EOF
    ok "punktfunk-web.service (port $WEB_PORT)"
fi

systemctl --user daemon-reload
loginctl show-user "$USER" 2>/dev/null | grep -q 'Linger=yes' || { sudo loginctl enable-linger "$USER" 2>/dev/null && ok "enabled linger (services run without login)" || warn "could not enable linger — services stop when you log out (sudo loginctl enable-linger $USER)"; }
# enable + restart (not `enable --now`): restart picks up unit-file changes on a re-run, where
# `--now` would no-op against an already-running service.
systemctl --user enable punktfunk-host.service 2>/dev/null
systemctl --user restart punktfunk-host.service
ok "punktfunk-host started"
if [ "$WITH_WEB" = 1 ]; then
    # The host writes the mgmt token on first start; give it a moment so the web unit finds it.
    for _ in $(seq 1 10); do [ -f "$CONFIG/mgmt-token" ] && break; sleep 0.5; done
    systemctl --user enable punktfunk-web.service 2>/dev/null
    systemctl --user restart punktfunk-web.service
    ok "punktfunk-web started"
fi

# --- 6. summary ------------------------------------------------------------
IP="$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | head -1 || true)"
echo
log "Done — punktfunk host is running on this Steam Deck"
echo "  • Host status:   systemctl --user status punktfunk-host"
if [ "$WITH_WEB" = 1 ]; then
    echo "  • Web console:   http://${IP:-steamdeck.local}:$WEB_PORT   (login: see $CONFIG/web.env)"
    echo "  • Pair a device: open the web console → Devices → arm pairing → enter the PIN on the client"
fi
if [ "$OPEN" = 1 ]; then
    echo "  • Mode: --open (unpaired clients accepted — trusted LAN only)"
else
    echo "  • Pairing required (secure default). From a client, pick this host and enter the PIN the host shows."
fi
echo "  • Update later:  bash $SRC/scripts/steamdeck/update.sh"
if [ "$NEED_RELOGIN" = 1 ]; then
    echo
    warn "ONE MORE STEP before streaming — reboot the Deck (or fully log out and back in)."
    echo "     KWin only authorizes Desktop-mode screen capture on a fresh session, and the new 'input'"
    echo "     group (native Steam Deck controller passthrough) only applies to a new login. Streaming"
    echo "     Game Mode with a generic Xbox pad works now; Desktop capture + the native Deck pad need the reboot."
fi
