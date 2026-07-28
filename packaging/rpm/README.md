# punktfunk-host — RPM (Bazzite / Fedora Atomic) via the Gitea registry

`punktfunk-host` is published as an RPM to **Gitea's RPM package registry** in the public `unom`
org (stable groups `bazzite`/`fedora-44`, canary groups `bazzite-canary`/`fedora-44-canary`), so
Bazzite / Fedora Atomic hosts layer and update it with `rpm-ostree`. CI (`.gitea/workflows/rpm.yml`)
builds and publishes on every push to `main` (a rolling `<next-minor>-0.ciN.g<sha>` build — the base
is derived from the latest stable tag by `scripts/ci/pf-version.sh` — to the `*-canary`
groups) and on `vX.Y.Z` tags (a clean `X.Y.Z-1` to the base groups, plus attached to the unified
Gitea Release) — separate repos, so a stable box never jumps to a canary build (see
[Release Channels](https://punktfunk.unom.io/docs/channels)). The `baseurl` below subscribes to the
`bazzite` stable group; use `bazzite-canary` for the latest main builds. The RPM is built in the
Fedora 43 image (`ci/fedora-rpm.Dockerfile`) so its auto-generated library Requires
(`libavcodec.so.NN`, …) match Bazzite's sonames; the NVIDIA driver lib (`libcuda.so.1`) is
excluded — NVENC/EGL come from whatever NVIDIA stack the host runs (a weak Recommends).

This is the same package as the [COPR](../copr/README.md) / [bootc](../bootc/Containerfile)
paths — same spec (`punktfunk.spec`) — just self-hosted in Gitea instead of COPR, mirroring the
[Debian/apt](../debian/README.md) setup.

## Install on a Bazzite host (one-time)

```sh
# Add the repo. Packages are GPG-signed (gpgcheck=1, the packages@unom.io key) AND the repo
# metadata is Gitea-signed (repo_gpgcheck=1); gpgkey lists both so dnf/rpm-ostree imports each.
sudo tee /etc/yum.repos.d/punktfunk.repo >/dev/null <<'REPO'
[gitea-unom-bazzite]
name=punktfunk (unom, Bazzite)
baseurl=https://git.unom.io/api/packages/unom/rpm/bazzite
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=https://git.unom.io/api/packages/unom/rpm/repository.key
       https://git.unom.io/api/packages/unom/generic/punktfunk-keys/1/RPM-GPG-KEY-punktfunk
REPO

# Layer the host + the web console (pairing/status), then reboot into the new deployment.
# (punktfunk Recommends punktfunk-web; list it explicitly so it's pulled regardless of weak-dep
# settings. The registry carries punktfunk-web because CI builds the spec --with web; COPR can't.)
rpm-ostree install punktfunk punktfunk-web
systemctl reboot
```

> If `rpm-ostree` can't complete the metadata GPG check non-interactively, set `repo_gpgcheck=0`
> (TLS-only trust to the self-hosted registry).

## Per-package signing (`gpgcheck=1`, active)

CI GPG-signs every RPM: `packaging/rpm/sign-rpms.sh` (run from `rpm.yml` between build and publish)
signs with the dedicated EdDSA key **`packages@unom.io`** (`AF245C506F4E4763`) and self-verifies
with `rpmkeys --checksig` before publishing, so an unsigned/bad build never reaches the registry.
The public key is served from the registry (the `gpgkey=` URL above) and committed at
`packaging/rpm/RPM-GPG-KEY-punktfunk`. (This is a GPG/OpenPGP key — a `step-ca`/X.509 cert can't
sign RPMs; step-ca is only for registry/console TLS.)

> `RPM_GPG_PRIVATE_KEY` is an **org-level** secret on `unom`, not a repo secret — it will not show
> up under this repository's Actions secrets. Verify it end to end instead of by its absence there:
> `curl -O <repo-url>/package/punktfunk-web/<ver>/x86_64/…rpm && rpm -qp --qf '%{RSAHEADER:pgpsig}\n'`
> (or `rpmkeys --checksig`, which reports `NOKEY` until you import the public key — `NOKEY` still
> means *signed*, just by a key that box doesn't have yet).

On a `v*` tag build, a missing key **fails** the build: `sign-rpms.sh` will not publish unsigned
RPMs into a repo whose own instructions say `gpgcheck=1`, because every user's `dnf upgrade` would
then break on them. Non-release builds still fall through unsigned so forks and local builds work.

How it was set up (and how to rotate the key):

```sh
# 1. Generate a DEDICATED, passphrase-less signing key (separate from the Gitea metadata key).
gpg --batch --gen-key <<EOF
%no-protection
Key-Type: eddsa
Key-Curve: ed25519
Name-Real: punktfunk packages
Name-Email: packages@unom.io
Expire-Date: 0
%commit
EOF
gpg --armor --export-secret-keys packages@unom.io   # -> the RPM_GPG_PRIVATE_KEY CI secret
gpg --armor --export             packages@unom.io > packaging/rpm/RPM-GPG-KEY-punktfunk  # public half

# 2. Add the armored PRIVATE key as the RPM_GPG_PRIVATE_KEY Gitea Actions secret, at the ORG level
#    (git.unom.io/org/unom/settings/actions/secrets) so every repo's workflows inherit it. Commit
#    the public half and publish it to the registry so the gpgkey= URL resolves:
curl --user "<user>:<write:package-PAT>" --upload-file packaging/rpm/RPM-GPG-KEY-punktfunk \
  https://git.unom.io/api/packages/unom/generic/punktfunk-keys/1/RPM-GPG-KEY-punktfunk
```

Rotating the key means a new generic-registry version (bump `punktfunk-keys/1` → `/2` and the
`gpgkey=` URL), since the registry rejects re-uploading an existing file.

After reboot, as the desktop user:

```sh
ujust add-user-to-input-group           # virtual gamepads need /dev/uinput (re-login).
                                        # Bazzite is atomic — use ujust, NOT `usermod -aG input`.
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.bazzite ~/.config/punktfunk/host.env   # gamescope defaults
systemctl --user enable --now punktfunk-host
# Web console — enable it and read the auto-generated login password (then open https://<host-ip>:47992):
systemctl --user enable --now punktfunk-web
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
```

(See [`../bazzite/README.md`](../bazzite/README.md) for the full appliance walkthrough —
udev/group, `host.env`, the Steam session unit, firewall, verify.)

## Updates

```sh
rpm-ostree upgrade            # pulls the newest punktfunk with the system update
systemctl reboot             # rpm-ostree changes apply on reboot
```

Layered packages are re-resolved against their repos on every `rpm-ostree upgrade`, so the box
tracks new builds automatically (Bazzite's auto-update timer does this for you). To pin or stop
tracking: `rpm-ostree override` / `rpm-ostree uninstall punktfunk`.

## Build an RPM locally

```sh
PF_VERSION=0.0.1 bash packaging/rpm/build-rpm.sh                # host + client
PF_VERSION=0.0.1 PF_WITH_WEB=1 bash packaging/rpm/build-rpm.sh  # + punktfunk-web (needs bun on PATH)
# -> dist/punktfunk-0.0.1-1.fcNN.x86_64.rpm  (+ punktfunk-web-0.0.1-1.fcNN.x86_64.rpm with PF_WITH_WEB=1;
#    the web subpackage vendors a bun binary, so it's arch-specific, not noarch)
```

Run it inside the Fedora 43 builder image so the deps resolve and match Bazzite:

```sh
docker build -f ci/fedora-rpm.Dockerfile -t punktfunk-fedora-rpm ci
docker run --rm -v "$PWD:/src" -w /src punktfunk-fedora-rpm \
  bash -lc 'git config --global --add safe.directory /src && PF_VERSION=0.0.1 bash packaging/rpm/build-rpm.sh'
```

A plain `rpmbuild`/COPR build with no `pf_version`/`pf_release` defines produces `0.3.0-1` (the
spec defaults).

### aarch64 — the client RPM

The **client** builds for aarch64; the **host** does not (its encode stack is NVENC/QSV/AMF, all
x86). `PF_WITHOUT_HOST=1` drops the host binary, the tray, the headless-session data, the
firewalld services and the main package's `%files`, leaving exactly one RPM: `punktfunk-client`.
Omitting the main `%files` is what keeps rpm from emitting an empty `punktfunk` next to it.

This is **not** a cross-compile — `%build` runs cargo for the host architecture, so run it on an
arm64 machine (or an emulated arm64 container, which is very slow):

```sh
docker build --platform linux/arm64 -f ci/fedora-rpm.Dockerfile -t punktfunk-fedora-rpm-arm64 ci
docker run --rm --platform linux/arm64 -v "$PWD:/src" -w /src punktfunk-fedora-rpm-arm64 \
  bash -lc 'git config --global --add safe.directory /src && \
            PF_VERSION=0.0.1 PF_WITHOUT_HOST=1 bash packaging/rpm/build-rpm.sh'
# -> dist/punktfunk-client-0.0.1-1.fcNN.aarch64.rpm
```

`PF_WITHOUT_HOST=1` works on x86_64 too, if you only want the client RPM. The flag is orthogonal
to the architecture; it is just that aarch64 has no other option.
