#!/usr/bin/env bash
set -Eeuo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
repo=$(cd "$here/../../.." && pwd -P)
die() { printf '%s\n' "$*" >&2; exit 1; }
log() { printf '%s\n' "$*"; }
require_command() { command -v "$1" >/dev/null || die "Missing command: $1"; }
[[ $EUID -eq 0 ]] || die 'Run as root on Linux.'
tag=${1:?Usage: build-image.sh TAG BUNDLE [OUTPUT_DIRECTORY]}
bundle=$(realpath "${2:?A release bundle is required}")
output_dir=$(realpath -m "${3:-$repo/dist}")
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+-rpi5\.[0-9]+$ ]] || die 'Invalid Raspberry Pi release tag.'
release_commit=$(git -C "$repo" rev-parse "$tag^{commit}")
[[ $(git -C "$repo" rev-parse HEAD) == "$release_commit" ]] || die 'Check out the release tag before building.'
base_url=https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2026-09-15/2026-09-15-raspios-trixie-arm64-lite.img.xz
base_sha=cdf4f3bfac35ae947b46e4e767f935453810549779ac3290e05a6754aee627e5
cache=${PUNKTFUNK_IMAGE_CACHE:-/root/.cache/punktfunk-raspios}
output="$output_dir/punktfunk-${tag#v}-kiosk.img"
[[ ! -e "$output.xz" ]] || die "Output already exists: $output"
for command in curl xz sfdisk losetup resize2fs e2fsck fsck.vfat python3; do require_command "$command"; done
mkdir -p "$cache" "$output_dir"
if [[ ! -f "$cache/base.img.xz" ]]; then
    curl -fL --retry 3 "$base_url" -o "$cache/base.img.xz.tmp"
    mv "$cache/base.img.xz.tmp" "$cache/base.img.xz"
fi
printf '%s  %s\n' "$base_sha" "$cache/base.img.xz" | sha256sum -c -
(cd "$(dirname "$bundle")" && sha256sum -c "$(basename "$bundle").sha256")
work=$(mktemp -d "$cache/build.XXXXXX")
root="$work/root"
loop=
mounts=()
cleanup() {
    for ((i=${#mounts[@]}-1; i>=0; i--)); do
        if mountpoint -q "${mounts[i]}"; then umount "${mounts[i]}" || return; fi
    done
    [[ -z "$loop" ]] || losetup -d "$loop"
    echo "Build workspace: $work"
}
trap cleanup EXIT
mount_track() { mount "$@"; mounts+=("${*: -1}"); }
mkdir -p "$root" "$work/bundle"
log 'Unpacking the pinned Raspberry Pi OS Lite image.'
xz -dc "$cache/base.img.xz" >"$work/image.img"
truncate -s 3600M "$work/image.img"
# Extend only partition 2; preserve the Pi firmware partition and PARTUUIDs.
printf ', +\n' | sfdisk --no-reread -N 2 "$work/image.img"
loop=$(losetup --find --show --partscan "$work/image.img")
e2fsck -pf "${loop}p2" || [[ $? == 1 ]]
resize2fs "${loop}p2"
mount_track "${loop}p2" "$root"
mount_track "${loop}p1" "$root/boot/firmware"
mount_track --bind /dev "$root/dev"
mount_track --bind /dev/pts "$root/dev/pts"
mount_track -t proc proc "$root/proc"
mount_track -t sysfs sysfs "$root/sys"
emulator=()
if [[ $(uname -m) != aarch64 ]]; then
    require_command qemu-aarch64-static
    install -m 0755 "$(command -v qemu-aarch64-static)" "$root/usr/bin/qemu-aarch64-static"
    emulator=(/usr/bin/qemu-aarch64-static)
fi
run() { chroot "$root" "${emulator[@]}" /usr/bin/env -i HOME=/root PATH=/usr/sbin:/usr/bin:/sbin:/bin DEBIAN_FRONTEND=noninteractive "$@"; }
if [[ ! -e "$work/resolv.conf" && ! -L "$work/resolv.conf" ]]; then
    cp -a "$root/etc/resolv.conf" "$work/resolv.conf"
fi
rm -f "$root/etc/resolv.conf"
cp -L /etc/resolv.conf "$root/etc/resolv.conf"
printf '#!/bin/sh\nexit 101\n' >"$root/usr/sbin/policy-rc.d"
chmod 0755 "$root/usr/sbin/policy-rc.d"
log 'Installing the Wayland, Vulkan and audio runtime without recommended packages.'
run apt-get update
run apt-get install -y --no-install-recommends \
    weston=14.0.2-1 libweston-14-0=14.0.2-1 libgl1-mesa-dri mesa-vulkan-drivers libvulkan1 libwayland-client0 libxkbcommon0 \
    libfontconfig1 libfreetype6 libopus0 libasound2t64 libpipewire-0.3-0 \
    libva-drm2 libva-x11-2 libdrm2 libgbm1 libstdc++6 libudev1 \
    pipewire wireplumber libspa-0.2-bluetooth rtkit dbus libpam-systemd network-manager bluez v4l-utils
run apt-get clean
bash "$here/install-weston-cursor-backend.sh" "$root"
rm -rf "$root/var/lib/apt/lists/"*
tar -xzf "$bundle" -C "$work/bundle" --strip-components=1
[[ $(cat "$work/bundle/build-version") == "${tag#v}" ]] || die 'Bundle version does not match release tag.'
[[ $(cat "$work/bundle/fork-commit") == "$release_commit" ]] || die 'Bundle commit does not match release tag.'
install -d "$root/opt/punktfunk-rpi5"
cp -a "$work/bundle/." "$root/opt/punktfunk-rpi5/"
if ! run id punktfunk >/dev/null 2>&1; then
    run useradd --system --user-group --home-dir /data --shell /usr/sbin/nologin punktfunk
fi
run usermod -a -G video,render,input,audio,netdev,bluetooth punktfunk
install -d "$root/data/.config/punktfunk" "$root/data/cache" "$root/data/state" "$root/data/logs"
install -m 0600 "$here/runtime/client-gtk-settings.json" "$root/data/.config/punktfunk/"
run chown -R punktfunk:punktfunk /data
# Public appliances have no provisioned remote account or password login.
run passwd --lock root
run passwd --lock punktfunk
install -D -m 0755 "$here/runtime/punktfunk-diagnostics" "$root/usr/local/bin/punktfunk-diagnostics"
sed -i 's/\r$//' "$root/usr/local/bin/punktfunk-diagnostics"
units="$root/etc/systemd/system"
for name in dbus weston pipewire wireplumber; do
    install -m 0644 "$here/runtime/punktfunk-$name.service" "$units/"
done
install -m 0644 "$here/runtime/punktfunk-kiosk.service" "$units/"
install -D -m 0755 "$here/runtime/wait-for-wayland" "$root/usr/libexec/punktfunk/wait-for-wayland"
sed -i 's/\r$//' "$units"/punktfunk-*.service
install -d "$units/punktfunk-kiosk.service.d"
cat >"$units/punktfunk-kiosk.service.d/diagnostics.conf" <<'EOF'
[Service]
Environment=RUST_LOG=info,pf_client_core::video=debug,pf_client_core::video_v4l2_request=trace,pf_client_core::session=debug,pf_client_core::audio=debug
StandardOutput=journal
StandardError=journal
LogRateLimitIntervalSec=0
EOF
install -d "$root/etc/default"
cat >"$root/etc/default/punktfunk" <<'EOF'
HOME=/data
XDG_RUNTIME_DIR=/run/punktfunk
XDG_CONFIG_HOME=/data/.config
XDG_CACHE_HOME=/data/cache
XDG_STATE_HOME=/data/state
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/punktfunk/bus
WAYLAND_DISPLAY=/run/punktfunk/wayland-0
EOF
cat >"$units/punktfunk-prepare.service" <<'EOF'
[Unit]
Description=Prepare Punktfunk runtime directory
Before=punktfunk-dbus.service punktfunk-weston.service punktfunk-pipewire.service
[Service]
Type=oneshot
ExecStart=/usr/bin/install -d -o punktfunk -g punktfunk -m 0700 /run/punktfunk
RemainAfterExit=yes
EOF
cat >"$units/punktfunk-session.target" <<'EOF'
[Unit]
Description=Punktfunk appliance session
Requires=punktfunk-prepare.service punktfunk-dbus.service punktfunk-weston.service
Wants=punktfunk-kiosk.service punktfunk-pipewire.service punktfunk-wireplumber.service
Wants=NetworkManager.service bluetooth.service
After=local-fs.target systemd-user-sessions.service NetworkManager.service
[Install]
WantedBy=graphical.target
EOF
install -d "$root/etc/xdg/weston" "$units/punktfunk-pipewire.service.d" "$root/etc/pipewire/pipewire.conf.d"
cat >"$root/etc/xdg/weston/weston.ini" <<'EOF'
[core]
repaint-window=12
shell=kiosk-shell.so
idle-time=0
require-input=false
[keyboard]
keymap_layout=se
EOF
cat >"$units/punktfunk-pipewire.service.d/realtime.conf" <<'EOF'
[Service]
AmbientCapabilities=CAP_SYS_NICE
CapabilityBoundingSet=CAP_SYS_NICE
LimitRTPRIO=88
LimitMEMLOCK=infinity
EOF
cat >"$root/etc/pipewire/pipewire.conf.d/kiosk.conf" <<'EOF'
context.properties = {
    default.clock.rate = 48000
    default.clock.allowed-rates = [ 48000 ]
    default.clock.quantum = 1024
    default.clock.min-quantum = 1024
    default.clock.max-quantum = 1024
}
EOF
install -d "$root/etc/NetworkManager/system-connections"
cat >"$root/etc/NetworkManager/system-connections/kiosk-wired.nmconnection" <<'EOF'
[connection]
id=Kiosk Wired
type=ethernet
autoconnect=true
[ipv4]
method=auto
[ipv6]
method=auto
EOF
chmod 0600 "$root/etc/NetworkManager/system-connections/kiosk-wired.nmconnection"
printf 'punktfunk-pi\n' >"$root/etc/hostname"
sed -i 's/127.0.1.1.*/127.0.1.1 punktfunk-pi/' "$root/etc/hosts"
run systemctl enable punktfunk-session.target NetworkManager.service bluetooth.service
run systemctl set-default graphical.target
run systemctl mask userconfig.service getty@tty1.service serial-getty@serial0.service
run systemctl mask ssh.service ssh.socket sshd.service regenerate_ssh_host_keys.service
rm -f "$root/boot/firmware/ssh" "$root/boot/firmware/ssh.txt"
install -m 0644 "$here/runtime/lite-runtime.conf" "$units/punktfunk-kiosk.service.d/runtime.conf"
install -m 0644 "$here/runtime/punktfunk-clock-ready.service" "$units/"
sed -i 's/\r$//' "$units/punktfunk-kiosk.service.d/runtime.conf" "$units/punktfunk-clock-ready.service"
run systemctl --global mask pipewire.service pipewire.socket wireplumber.service filter-chain.service
# The resize token retains Raspberry Pi OS's first-boot root expansion.
sed -i 's/console=tty1 //' "$root/boot/firmware/cmdline.txt"
if ! grep -q 'vt.global_cursor_default=0' "$root/boot/firmware/cmdline.txt"; then
    sed -i 's/$/ quiet loglevel=3 systemd.show_status=false vt.global_cursor_default=0/' "$root/boot/firmware/cmdline.txt"
fi
install -d "$root/etc/systemd/journald.conf.d"
printf '[Journal]\nStorage=persistent\nSystemMaxUse=32M\n' >"$root/etc/systemd/journald.conf.d/kiosk.conf"
printf 'BASE=raspios-trixie-2026-09-15\nRELEASE_TAG=%s\nPUNKTFUNK_VERSION=%s\nFORK_COMMIT=%s\n' "$tag" "${tag#v}" "$release_commit" >"$root/etc/punktfunk-kiosk-release"
log 'Checking runtime dependencies, service graph and image space.'
run /opt/punktfunk-rpi5/punktfunk --help >"$work/cli-help.txt"
run ldd /opt/punktfunk-rpi5/punktfunk-session >"$work/linkage.txt"
! grep -q 'not found' "$work/linkage.txt" || { cat "$work/linkage.txt"; die 'Missing session library'; }
run ldd -r /usr/lib/aarch64-linux-gnu/libweston-14/drm-backend.so >"$work/weston-linkage.txt"
! grep -E 'not found|undefined symbol' "$work/weston-linkage.txt" || die 'Weston module linkage failed.'
run systemd-analyze verify punktfunk-session.target punktfunk-kiosk.service
install -d "$root/etc/ssh"
find "$root/etc/ssh" -maxdepth 1 -type f -name 'ssh_host_*_key*' -delete
python3 "$here/verify-image.py" "$root" "$tag" "$release_commit"
run dpkg-query -W '-f=${binary:Package}\t${Version}\n' >"$work/packages.txt"
df -B1 "$root" >"$work/space.txt"
cat "$work/space.txt"
available=$(df -B1 --output=avail "$root" | tail -1)
[[ $available -ge 268435456 ]] || die 'Less than 256 MiB free in the finished image.'
rm -f "$root/usr/sbin/policy-rc.d" "$root/usr/bin/qemu-aarch64-static" "$root/etc/resolv.conf"
cp -a "$work/resolv.conf" "$root/etc/resolv.conf"
# Each flashed device establishes its own machine identity.
: >"$root/etc/machine-id"
rm -f "$root/var/lib/dbus/machine-id" "$root/var/lib/systemd/random-seed"
ln -s /etc/machine-id "$root/var/lib/dbus/machine-id"
sync
for ((i=${#mounts[@]}-1; i>=0; i--)); do umount "${mounts[i]}"; done
mounts=()
fsck.vfat -n "${loop}p1"
e2fsck -fn "${loop}p2"
losetup -d "$loop"
loop=
[[ $(stat -c %s "$work/image.img") -lt 4000000000 ]] || die 'Image exceeds 4 GB.'
xz -T0 -6 -c "$work/image.img" >"$output.xz.tmp"
[[ $(stat -c %s "$output.xz.tmp") -lt 2147483648 ]] || die 'Compressed image exceeds the GitHub asset size limit.'
xz -t "$output.xz.tmp"
mv "$output.xz.tmp" "$output.xz"
cp "$work/packages.txt" "$output.packages.txt"
{
    printf 'Release: %s\nCommit: %s\nBase: %s\nBase SHA256: %s\n' "$tag" "$release_commit" "$base_url" "$base_sha"
    printf 'Remote login: disabled; no authorized keys\n'
    sha256sum "$bundle" "$here/weston-14.0.2-rpi5-cursor-drm-backend.so"
    cat "$work/space.txt"
} >"$output.build.txt"
(cd "$output_dir" && sha256sum "$(basename "$output.xz")" "$(basename "$output.packages.txt")" "$(basename "$output.build.txt")" >"$(basename "$output").sha256")
log "Image complete: $output.xz"
