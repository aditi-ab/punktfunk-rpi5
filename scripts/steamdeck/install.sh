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
#   bash scripts/steamdeck/install.sh                 # secure default: PIN pairing required
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
WEB_PORT="${PUNKTFUNK_WEB_PORT:-3000}"
OPEN=0
WITH_WEB=1
for arg in "$@"; do
    case "$arg" in
        --open) OPEN=1 ;;
        --no-web) WITH_WEB=0 ;;
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
    build-essential pkg-config clang curl git ca-certificates \
    libavcodec-dev libavformat-dev libavutil-dev libavfilter-dev libswscale-dev libavdevice-dev \
    libpipewire-0.3-dev libspa-0.2-dev \
    libgbm-dev libegl-dev libgl-dev libdrm-dev libva-dev \
    libxkbcommon-dev libudev-dev libssl-dev libopus-dev libsdl2-dev \
    nodejs >/dev/null
command -v rustc >/dev/null 2>&1 || command -v ~/.cargo/bin/rustc >/dev/null 2>&1 || \
    curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path >/dev/null
# bun builds the web console; node runs it (the node-server preset; bun mis-resolves the Nitro
# externalized server deps like srvx at request time).
command -v bun >/dev/null 2>&1 || command -v ~/.bun/bin/bun >/dev/null 2>&1 || \
    curl -fsSL https://bun.sh/install | bash >/dev/null
'
ok "build deps ready"

# --- 2. build host (+ web) -------------------------------------------------
log "Building punktfunk-host (release) — first build is slow (~10-15 min)"
distrobox enter "$BOX" -- bash -lc "
set -e
export PATH=\$HOME/.cargo/bin:\$PATH CARGO_TARGET_DIR='$TARGET_DIR'
cd '$SRC' && cargo build -r -p punktfunk-host
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
# Auto encoder: VAAPI on the Deck's AMD GPU, NVENC on NVIDIA.
PUNKTFUNK_ENCODER=auto
# The host auto-detects the live session (Game Mode gamescope / Desktop KDE) per connect.
# Override the compositor only if detection misbehaves: PUNKTFUNK_COMPOSITOR=gamescope
EOF
    ok "wrote host.env"
else
    ok "host.env exists (left as-is)"
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

# --- 4. system tuning (needs sudo; skipped gracefully if unavailable) ------
log "System tuning (UDP buffers + input group) — needs sudo"
if sudo -n true 2>/dev/null; then
    printf 'net.core.wmem_max=33554432\nnet.core.rmem_max=33554432\n' \
        | sudo tee /etc/sysctl.d/99-punktfunk-net.conf >/dev/null
    sudo sysctl -q -p /etc/sysctl.d/99-punktfunk-net.conf >/dev/null
    ok "UDP socket buffers raised to 32 MB (persisted)"
    if [ -f "$SRC/scripts/60-punktfunk.rules" ]; then
        sudo install -m644 "$SRC/scripts/60-punktfunk.rules" /etc/udev/rules.d/60-punktfunk.rules
        sudo udevadm control --reload-rules && sudo udevadm trigger || true
        ok "installed udev rule (virtual gamepads)"
    fi
    id -nG "$USER" | grep -qw input || { sudo usermod -aG input "$USER"; warn "added $USER to 'input' group — log out/in (or reboot) for gamepad support"; }
else
    warn "passwordless sudo unavailable — skipping UDP-buffer + udev tuning."
    warn "Without it, high-bitrate streaming drops packets. Apply manually later:"
    warn "  echo -e 'net.core.wmem_max=33554432\\nnet.core.rmem_max=33554432' | sudo tee /etc/sysctl.d/99-punktfunk-net.conf && sudo sysctl --system"
fi

# --- 5. systemd user services ---------------------------------------------
log "Installing systemd user services"
mkdir -p "$UNITS"
# --gamestream keeps the Moonlight-compat planes (the Deck commonly streams to Moonlight too); drop
# it for a secure native-only host (no #5/#9 surface — native clients only).
SERVE_ARGS="serve --gamestream --mgmt-bind 0.0.0.0:$MGMT_PORT"
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
    # The console is a Nitro/Node server run by bun; it lives in the build container (bun + node
    # libs) and proxies to the host's loopback HTTPS mgmt API.
    cat > "$UNITS/punktfunk-web.service" <<EOF
# Generated by scripts/steamdeck/install.sh — punktfunk web console (bun in the '$BOX' distrobox).
[Unit]
Description=punktfunk management web console
After=punktfunk-host.service

[Service]
ExecStart=$DISTROBOX enter $BOX -- bash -lc 'cd $SRC/web; set -a; . $CONFIG/mgmt-token; . $CONFIG/web.env; set +a; export PUNKTFUNK_MGMT_URL=https://127.0.0.1:$MGMT_PORT NODE_TLS_REJECT_UNAUTHORIZED=0 PORT=$WEB_PORT HOST=0.0.0.0 NITRO_PORT=$WEB_PORT NITRO_HOST=0.0.0.0; exec node .output/server/index.mjs'
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
