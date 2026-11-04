#!/usr/bin/env bash
set -Eeuo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
[[ $EUID -eq 0 ]] || { printf 'Run as root on the Pi.\n' >&2; exit 1; }
[[ $(uname -m) == aarch64 && -f /etc/punktfunk-kiosk-release ]] || exit 1
source /etc/os-release
[[ ${VERSION_CODENAME:-} == trixie ]] || { printf 'This setup requires the Trixie kiosk OS.\n' >&2; exit 1; }
version=$(cat /opt/punktfunk-rpi5/build-version)
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+-rpi5\.[0-9]+$ ]] || exit 1
[[ ! -L /data ]] || exit 1

# A sticky, root-owned parent protects private service state while allowing app files.
install -d -o root -g punktfunk -m 1775 /data
for directory in /data/punktfunk-update /data/virtualhere; do
    if [[ -e $directory || -L $directory ]]; then
        [[ -d $directory && ! -L $directory && $(stat -c %u "$directory") == 0 ]] || exit 1
        [[ $(stat -c %a "$directory") == 700 ]] || exit 1
    else
        mkdir -m 0700 "$directory"
    fi
done

install -d -m 0755 /opt/punktfunk-releases
if [[ ! -L /opt/punktfunk-rpi5 ]]; then
    target="/opt/punktfunk-releases/$version"
    [[ ! -e $target ]] || exit 1
    active=0
    systemctl is-active --quiet punktfunk-kiosk.service && active=1
    systemctl stop punktfunk-kiosk.service
    mv /opt/punktfunk-rpi5 "$target"
    if ! ln -s "punktfunk-releases/$version" /opt/punktfunk-rpi5; then
        mv "$target" /opt/punktfunk-rpi5
        systemctl start punktfunk-kiosk.service
        exit 1
    fi
    [[ $active == 0 ]] || systemctl start punktfunk-kiosk.service
fi

visudo --check --file "$here/runtime/kiosk-sudoers"
for helper in kiosk-control kiosk-update; do
    install -D -m 0755 "$here/runtime/$helper" "/usr/libexec/punktfunk/$helper"
    sed -i 's/\r$//' "/usr/libexec/punktfunk/$helper"
done
install -m 0440 "$here/runtime/kiosk-sudoers" /etc/sudoers.d/punktfunk-kiosk
sed -i 's/\r$//' /etc/sudoers.d/punktfunk-kiosk
for unit in punktfunk-virtualhere.service punktfunk-update.service punktfunk-update-recovery.service; do
    install -m 0644 "$here/runtime/$unit" "/etc/systemd/system/$unit"
    sed -i 's/\r$//' "/etc/systemd/system/$unit"
done
install -D -m 0644 "$here/runtime/kiosk-update.conf" /etc/systemd/system/punktfunk-kiosk.service.d/update.conf
sed -i 's/\r$//' /etc/systemd/system/punktfunk-kiosk.service.d/update.conf
printf 'raspios-trixie-arm64-v1\n' >/etc/punktfunk-kiosk-runtime
systemctl daemon-reload
systemctl start punktfunk-update-recovery.service
printf 'Kiosk tools installed. Punktfunk settings and the current app were kept.\n'
