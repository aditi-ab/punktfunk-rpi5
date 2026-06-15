#!/usr/bin/env bash
# Detached-GPG-sign the built dist/*.rpm so the Gitea RPM registry can be served with gpgcheck=1.
#
# DORMANT by default: if RPM_GPG_PRIVATE_KEY is unset this exits 0 and leaves the RPMs unsigned —
# exactly today's behaviour — so it is SAFE to ship before a key exists. The signing only activates
# once you add the key as a CI secret (see packaging/rpm/README.md "Enabling per-package signing").
#
# Requires a DEDICATED, PASSPHRASE-LESS signing key (the one the runbook generates with
# %no-protection), distinct from the Gitea instance's repo-metadata key — rpm's default signer
# can't supply a passphrase non-interactively here.
#
# Usage (in rpm.yml, after build-rpm.sh): RPM_GPG_PRIVATE_KEY=... bash packaging/rpm/sign-rpms.sh
set -euo pipefail

if [ -z "${RPM_GPG_PRIVATE_KEY:-}" ]; then
  echo "RPM_GPG_PRIVATE_KEY unset — leaving dist/*.rpm UNSIGNED (registry stays gpgcheck=0)."
  exit 0
fi

command -v rpmsign >/dev/null 2>&1 || dnf -y install rpm-sign >/dev/null

GNUPGHOME="$(mktemp -d)"; export GNUPGHOME; chmod 700 "$GNUPGHOME"
trap 'rm -rf "$GNUPGHOME"' EXIT
# Non-interactive in CI (no TTY): force loopback pinentry via gpg.conf so even rpm's default
# signing macro's gpg call won't try to prompt. The passphrase-less key needs no prompt anyway.
printf 'pinentry-mode loopback\n' > "$GNUPGHOME/gpg.conf"
printf 'allow-loopback-pinentry\n' > "$GNUPGHOME/gpg-agent.conf"

printf '%s' "$RPM_GPG_PRIVATE_KEY" | gpg --batch --import
KEYID="$(gpg --list-secret-keys --with-colons | awk -F: '/^sec:/{print $5; exit}')"
[ -n "$KEYID" ] || { echo "no secret key imported from RPM_GPG_PRIVATE_KEY" >&2; exit 1; }

# Sign with rpm's DEFAULT __gpg_sign_cmd — it expands %{__signature_filename}/%{__plaintext_filename}
# correctly. (A custom __gpg_sign_cmd passed via --define reached gpg with those filename macros
# UNEXPANDED -> "No such file or directory".) Just point rpm at our key; the GNUPGHOME above
# (passphrase-less key + loopback) lets gpg sign headless.
for rpm in dist/*.rpm; do
  rpmsign --define "_gpg_name $KEYID" --addsign "$rpm"
done

# Verify locally so a bad signature fails the build before publishing.
rpm --import <(gpg --export --armor "$KEYID")
rpmkeys --checksig dist/*.rpm
echo "signed + verified $(find dist -name '*.rpm' | wc -l) RPM(s) with key $KEYID"
