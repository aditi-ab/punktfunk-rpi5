#!/usr/bin/env bash
# Detached-GPG-sign the built dist/*.rpm so the Gitea RPM registry can be served with gpgcheck=1.
#
# DORMANT by default: if RPM_GPG_PRIVATE_KEY is unset this exits 0 and leaves the RPMs unsigned —
# exactly today's behaviour — so it is SAFE to ship before a key exists. The signing only activates
# once you add the key as a CI secret (see packaging/rpm/README.md "Enabling per-package signing").
#
# Recommended: a DEDICATED, PASSPHRASE-LESS signing key (simplest in CI), distinct from the Gitea
# instance's repo-metadata key. If your key has a passphrase, set RPM_GPG_PASSPHRASE too.
#
# Usage (in rpm.yml, after build-rpm.sh): RPM_GPG_PRIVATE_KEY=... [RPM_GPG_PASSPHRASE=...] bash packaging/rpm/sign-rpms.sh
set -euo pipefail

if [ -z "${RPM_GPG_PRIVATE_KEY:-}" ]; then
  echo "RPM_GPG_PRIVATE_KEY unset — leaving dist/*.rpm UNSIGNED (registry stays gpgcheck=0)."
  exit 0
fi

command -v rpmsign >/dev/null 2>&1 || dnf -y install rpm-sign >/dev/null

GNUPGHOME="$(mktemp -d)"; export GNUPGHOME; chmod 700 "$GNUPGHOME"
trap 'rm -rf "$GNUPGHOME"' EXIT
echo "allow-loopback-pinentry" > "$GNUPGHOME/gpg-agent.conf"

printf '%s' "$RPM_GPG_PRIVATE_KEY" | gpg --batch --import
KEYID="$(gpg --list-secret-keys --with-colons | awk -F: '/^sec:/{print $5; exit}')"
[ -n "$KEYID" ] || { echo "no secret key imported from RPM_GPG_PRIVATE_KEY" >&2; exit 1; }

# rpm v4 detached-signing macro. NOTE: %{__gpg} already IS the gpg binary path — do NOT add a
# literal `gpg` after it (that becomes a spurious filename arg -> "no command supplied"). Force
# loopback pinentry (no TTY in CI); feed the passphrase, if any, on stdin via --passphrase-fd 0.
SIGN_CMD="%{__gpg} --batch --no-verbose --no-armor --pinentry-mode loopback"
[ -n "${RPM_GPG_PASSPHRASE:-}" ] && SIGN_CMD="$SIGN_CMD --passphrase-fd 0"
SIGN_CMD="$SIGN_CMD -u %{_gpg_name} --digest-algo sha256 -sbo %{__signature_filename} %{__plaintext_filename}"

for rpm in dist/*.rpm; do
  printf '%s' "${RPM_GPG_PASSPHRASE:-}" | rpmsign \
    --define "_gpg_name $KEYID" \
    --define "__gpg_sign_cmd $SIGN_CMD" \
    --addsign "$rpm"
done

# Verify locally so a bad signature fails the build before publishing.
rpm --import <(gpg --export --armor "$KEYID")
rpmkeys --checksig dist/*.rpm
echo "signed + verified $(find dist -name '*.rpm' | wc -l) RPM(s) with key $KEYID"
