#!/usr/bin/env bash
# punktfunk-sysext — install/update the punktfunk host on Bazzite / Fedora Atomic as a
# systemd-sysext, the no-layering path (rpm-ostree layering is a last resort per the Bazzite
# docs: it slows every update and can block upgrades; a sysext never enters an rpm-ostree
# transaction, needs no reboot, and is trivially removable).
#
# The image overlays /usr from /var/lib/extensions/punktfunk.raw with the host, tray and web
# console + their udev/sysctl/systemd-user payload; the RPMs' two /etc files (gamescope
# session drop-in, tray autostart) ride inside at /usr/share/punktfunk/etc/ and are copied
# into the real /etc here (a sysext can only carry /usr).
#
# Bootstrap (the script also ships inside the image as /usr/bin/punktfunk-sysext):
#   curl -fsSLO https://git.unom.io/unom/punktfunk/raw/branch/main/packaging/bazzite/punktfunk-sysext.sh
#   sudo bash punktfunk-sysext.sh install            # or: install --channel canary
# Thereafter:
#   sudo punktfunk-sysext update | status | remove
#
# Feed: the Gitea generic package registry, one feed per Fedora major x channel
# (…/punktfunk-sysext/f43/, f43-canary, f44, …), each a SHA256SUMS + versioned .raw files —
# published by .gitea/workflows/rpm.yml from the same RPMs the (legacy) layering path uses.
# The image pins ID=fedora + VERSION_ID, so after a major OS rebase the old image is refused
# (not merged broken) and `punktfunk-sysext update` re-resolves against the new release.
set -euo pipefail

REGISTRY="${PUNKTFUNK_SYSEXT_REGISTRY:-https://git.unom.io/api/packages/unom/generic/punktfunk-sysext}"
CONF=/etc/punktfunk-sysext.conf
EXT_DIR=/var/lib/extensions
IMG="$EXT_DIR/punktfunk.raw"
SIDECAR="$EXT_DIR/.punktfunk.version"
MARKER=/usr/lib/extension-release.d/extension-release.punktfunk
ETC_SRC=/usr/share/punktfunk/etc

usage() {
  sed -n 's/^#\( \|$\)//p' "$0" | sed -n '1,20p'
  echo "usage: punktfunk-sysext install [--channel stable|canary] [--from-file X.raw]"
  echo "       punktfunk-sysext update [--from-file X.raw] | status | remove"
  exit "${1:-0}"
}
need_root() { [ "$(id -u)" = 0 ] || { echo "run as root (sudo)" >&2; exit 1; }; }

os_version_id() { . /etc/os-release; echo "${VERSION_ID%%.*}"; }
channel() { # shellcheck disable=SC1090
  [ -f "$CONF" ] && . "$CONF"; echo "${CHANNEL:-stable}"; }
feed_url() {
  local suffix=""
  [ "$(channel)" = canary ] && suffix="-canary"
  echo "$REGISTRY/f$(os_version_id)$suffix"
}

# latest -> "VERSION FILENAME SHA256" from the feed's SHA256SUMS (highest by version sort).
latest() {
  local feed; feed="$(feed_url)"
  curl -fsSL "$feed/SHA256SUMS" \
    | awk '$2 ~ /^punktfunk-.*-x86-64\.raw$/ { v=$2; sub(/^punktfunk-/,"",v); sub(/-x86-64\.raw$/,"",v); print v, $2, $1 }' \
    | sort -V | tail -n1
}

installed_version() {
  if [ -f "$MARKER" ]; then
    sed -n 's/^SYSEXT_VERSION_ID=//p' "$MARKER"
  elif [ -f "$SIDECAR" ]; then
    cat "$SIDECAR"
  fi
}
merged() { [ -f "$MARKER" ]; }

post_merge() {
  if ! merged; then
    echo "!! image installed but NOT merged — 'systemd-sysext status' / 'journalctl -u systemd-sysext'" >&2
    echo "!! (an OS release the image doesn't match? 'punktfunk-sysext update' fetches the right one)" >&2
    return 1
  fi
  # What the RPM scriptlets would have done: pick up the uinput/uhid rule + the UDP buffer
  # sysctl now, no reboot (both also auto-apply at boot once merged — the files live in /usr/lib).
  udevadm control --reload 2>/dev/null || :
  udevadm trigger --subsystem-match=misc 2>/dev/null || :
  for f in /usr/lib/sysctl.d/99-punktfunk-net.conf /usr/lib/sysctl.d/99-punktfunk-client-net.conf; do
    [ -f "$f" ] && sysctl -q -p "$f" 2>/dev/null || :
  done
  # vhci-hcd: the usbip transport that makes the virtual Steam Deck pad a real USB device Steam
  # Input adopts. Without it the pad falls back to plain UHID hid-steam, which Steam Input won't
  # promote (Interface: -1) — so on a host in Game Mode the controller never appears and you can't
  # navigate. Two things must be true at boot: the module loaded, and the vhci `attach`/`detach`
  # sysfs files opened to the `input` group (the host runs unprivileged and can't modprobe/chown).
  #
  # A sysext CANNOT rely on its own /usr/lib/modules-load.d + /usr/lib/udev files for this: the
  # image merges (systemd-sysext.service) AFTER systemd-modules-load and early udev have already
  # run, so at a plain reboot vhci-hcd is never loaded and its rule never applied. Mirror BOTH into
  # real /etc (read at the normal early-boot time, and shadowing the /usr copies by filename) so the
  # module loads early and udev's coldplug trigger grants the group access. Refreshed every merge so
  # a rule/module change in a new image propagates (neither is user-editable config). Then load +
  # (re)apply now, no reboot, for this session.
  install -Dm0644 /usr/lib/modules-load.d/punktfunk.conf /etc/modules-load.d/punktfunk.conf 2>/dev/null || :
  install -Dm0644 /usr/lib/udev/rules.d/60-punktfunk.rules /etc/udev/rules.d/60-punktfunk.rules 2>/dev/null || :
  udevadm control --reload 2>/dev/null || :
  modprobe vhci-hcd 2>/dev/null || :
  # Re-fire the vhci rule against the (possibly already-present) controller so attach/detach pick up
  # the input-group ownership even when the module's original add event predated the reloaded rule.
  udevadm trigger --subsystem-match=platform --sysname-match='vhci_hcd.*' 2>/dev/null || :
  # The /etc payload a sysext can't carry. The gamescope-session drop-in is %config(noreplace):
  # only seed it, never clobber a local edit. The tray autostart entry is not user config.
  if [ -f "$ETC_SRC/gamescope-session-plus/sessions.d/steam" ] \
     && [ ! -e /etc/gamescope-session-plus/sessions.d/steam ]; then
    install -Dm0644 "$ETC_SRC/gamescope-session-plus/sessions.d/steam" \
      /etc/gamescope-session-plus/sessions.d/steam
  fi
  if [ -f "$ETC_SRC/xdg/autostart/io.unom.Punktfunk.Tray.desktop" ]; then
    install -Dm0644 "$ETC_SRC/xdg/autostart/io.unom.Punktfunk.Tray.desktop" \
      /etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop
  fi
}

# do_install VERSION FILENAME SHA256 | do_install --from-file X.raw
do_install() {
  need_root
  mkdir -p "$EXT_DIR"
  local tmp="$EXT_DIR/.punktfunk.raw.new" ver
  if [ "$1" = --from-file ]; then
    ver="(local: $(basename "$2"))"
    cp -f "$2" "$tmp"
  else
    ver="$1"
    echo "downloading punktfunk $ver ($(channel), fedora $(os_version_id))…"
    curl -fL --progress-bar -o "$tmp" "$(feed_url)/$2"
    echo "$3  $tmp" | sha256sum -c --quiet
  fi
  mv -f "$tmp" "$IMG"          # marker inside is extension-release.punktfunk — name must match
  echo "$ver" > "$SIDECAR"
  systemctl enable --now systemd-sysext.service >/dev/null 2>&1 || :
  systemd-sysext refresh
  post_merge
  echo "punktfunk $ver merged into /usr."
}

layering_hint() {
  if command -v rpm-ostree >/dev/null 2>&1 \
     && rpm-ostree status 2>/dev/null | grep -q 'LayeredPackages:.*punktfunk'; then
    cat >&2 <<'EOF'
!! punktfunk is ALSO layered via rpm-ostree. The sysext now shadows it, but remove the
!! layer so it stops slowing/blocking OS updates (the reason this sysext exists):
!!     sudo rpm-ostree uninstall punktfunk punktfunk-web && systemctl reboot
EOF
  fi
}

cmd_install() {
  need_root
  local from_file=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --channel)   printf 'CHANNEL=%s\n' "${2:?}" > "$CONF"; shift 2 ;;
      --from-file) from_file="${2:?}"; shift 2 ;;
      *) usage 1 ;;
    esac
  done
  if [ -n "$from_file" ]; then
    do_install --from-file "$from_file"
  else
    local l; l="$(latest)"
    [ -n "$l" ] || { echo "no image in the feed $(feed_url)" >&2; exit 1; }
    # shellcheck disable=SC2086
    do_install $l
  fi
  layering_hint
  cat <<'EOF'

First-run (once):
  ujust add-user-to-input-group        # virtual gamepads; then log out + back in
  mkdir -p ~/.config/punktfunk
  cp /usr/share/punktfunk/host.env.bazzite ~/.config/punktfunk/host.env
  systemctl --user daemon-reload && systemctl --user enable --now punktfunk-host
Updates:  sudo punktfunk-sysext update
EOF
}

cmd_update() {
  need_root
  if [ "${1:-}" = --from-file ]; then do_install --from-file "${2:?}"; return; fi
  local cur l ver
  cur="$(installed_version)"
  l="$(latest)"
  [ -n "$l" ] || { echo "no image in the feed $(feed_url)" >&2; exit 1; }
  ver="${l%% *}"
  if [ "$ver" = "$cur" ] && merged; then
    echo "already on $cur (channel $(channel)) — nothing to do."
    return
  fi
  echo "updating: ${cur:-<none>} -> $ver"
  # shellcheck disable=SC2086
  do_install $l
  echo "restart the host to pick up the new binary:  systemctl --user restart punktfunk-host"
}

cmd_status() {
  echo "channel:    $(channel)"
  echo "feed:       $(feed_url)"
  echo "image:      $([ -f "$IMG" ] && du -h "$IMG" | cut -f1 || echo '(not installed)')"
  echo "merged:     $(merged && echo yes || echo no)"
  echo "installed:  $(installed_version || true)"
  echo "latest:     $(latest 2>/dev/null | cut -d' ' -f1 || true)"
}

cmd_remove() {
  need_root
  # /etc cleanup needs the /usr payload for the unmodified-compare — do it BEFORE unmerging.
  if merged; then
    if cmp -s "$ETC_SRC/gamescope-session-plus/sessions.d/steam" \
              /etc/gamescope-session-plus/sessions.d/steam 2>/dev/null; then
      rm -f /etc/gamescope-session-plus/sessions.d/steam
    fi
  fi
  rm -f /etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop
  rm -f "$IMG" "$SIDECAR" "$CONF"
  systemd-sysext refresh 2>/dev/null || :
  echo "punktfunk sysext removed (user config in ~/.config/punktfunk is untouched)."
}

case "${1:-}" in
  install) shift; cmd_install "$@" ;;
  update)  shift; cmd_update "$@" ;;
  status)  shift; cmd_status ;;
  remove)  shift; cmd_remove ;;
  *) usage ;;
esac
