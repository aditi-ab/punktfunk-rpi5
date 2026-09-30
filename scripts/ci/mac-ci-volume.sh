#!/bin/sh
# Attach the macOS runner's build volume: an APFS sparse image on home-central's ci-mini-mac share,
# mounted at /Volumes/pf-ci. Build paths reach it only through ~/ci symlinks, so a missing volume
# fails jobs instead of filling the internal disk. The share password is in ~/ci/.pf-ci-smb (0600).
#
# Usage: sh scripts/ci/mac-ci-volume.sh          attach if needed; exits 1 unless the volume is up
#        sh scripts/ci/mac-ci-volume.sh create   once: create the image on the share
set -eu
SHARE=192.168.1.10/ci-mini-mac
SMB="$HOME/.pf-ci-share"
IMG="$SMB/pf-ci.sparsebundle"
VOL=/Volumes/pf-ci

if ! mount | grep -q " on $SMB "; then
    mkdir -p "$SMB"
    mount_smbfs "//ci-mini:$(cat "$HOME/ci/.pf-ci-smb")@$SHARE" "$SMB"
fi
if [ "${1:-}" = create ] && [ ! -d "$IMG" ]; then
    # hdiutil create fails on an SMB path on macOS 27 ("RPC version wrong"); diskutil works.
    diskutil image create blank --format UDSB --size 400g --volumeName pf-ci --fs APFS "$IMG"
fi
mount | grep -q " on $VOL " || diskutil image attach --mountOptions nobrowse --mountPoint "$VOL" "$IMG" >/dev/null
[ "${1:-}" = create ] && touch "$VOL/.pf-ci"
test -f "$VOL/.pf-ci" || { echo "$VOL is not the CI volume" >&2; exit 1; }
