# COPR build-from-SCM settings

COPR builds the RPM from this git repo (no manual SRPM upload). Configure the project
once in the COPR web UI (or with `copr-cli`):

**Project → New Build → SCM**
- Clone URL:      `https://git.unom.io/unom/punktfunk`
- Committish:     `main` (or a release tag)
- Subdirectory:   *(repo root)*
- Spec File:      `packaging/rpm/punktfunk.spec`
- Source build method: `rpkg` (or `make_srpm`)

**Project settings**
- Chroots: `fedora-41-x86_64`, `fedora-42-x86_64` (match your Bazzite Fedora base;
  `rpm -E %fedora` on the host tells you which). Add `aarch64` if needed.
- External repositories (so `ffmpeg-devel` resolves at build time):
  `https://mirrors.rpmfusion.org/nonfree/fedora/rpmfusion-nonfree-release-$releasever.noarch.rpm`
  and the matching `-free-` repo.
- Enable network during build (cargo fetches crates from crates.io) — COPR allows this by
  default.

`copr-cli` equivalent:

```sh
copr-cli create punktfunk --chroot fedora-42-x86_64 \
  --repo 'https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$releasever.noarch.rpm' \
  --repo 'https://mirrors.rpmfusion.org/nonfree/fedora/rpmfusion-nonfree-release-$releasever.noarch.rpm'
copr-cli buildscm punktfunk \
  --clone-url https://git.unom.io/unom/punktfunk \
  --commit main --spec packaging/rpm/punktfunk.spec --method rpkg
```

Note: COPR caps build time/RAM; a full `cargo build --release` of the host (FFmpeg/PipeWire
sys-crates + aws-lc-rs) is heavy but within the default COPR limits. If a chroot OOMs, lower
parallelism with `CARGO_BUILD_JOBS` in the spec's `%build`.

## The web console subpackage (`punktfunk-web`)

The spec can also build the management web console as a noarch `punktfunk-web` subpackage, but it's
gated behind `%bcond_with web` and **OFF by default** — building the Nitro/Node SSR bundle needs
`bun`, which COPR's mock chroot does not provide. So a stock COPR build produces only `punktfunk`
+ `punktfunk-client`.

Two ways to get the console:
- **Recommended:** install it from the Gitea RPM registry (`packaging/rpm/README.md`, Option A),
  whose CI builder image has `bun` and builds `--with web`. (This is what `bootc/Containerfile` does.)
- **In COPR:** add `bun` to the chroot (a custom mock config / external repo) and set the build
  option `--with web` on the project, then `dnf install punktfunk-web`.
