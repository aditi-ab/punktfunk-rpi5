# Raspberry Pi app updater

Release: `v0.46.0-rpi5.30`
Source: `d65d8519bd90aabdd7a34d1fe15f4cf03e66cd7a`
Upstream base: `v0.46.0`
Exact replay tree: `a6ed258191ccbe313155aedd36dd0fc83639ceaa`
Public release: https://github.com/aditi-ab/punktfunk-rpi5/releases/tag/v0.46.0-rpi5.30

## Delivery

The Raspberry Pi settings category has a Punktfunk updates page. GitHub releases
include a complete portable app archive and a versioned update manifest. The
runtime contract is `raspios-trixie-arm64-v1`; updates leave OS packages, kernel,
firmware and compositor untouched. Root helpers are installed separately by the
image builder or the one-time `install-kiosk-tools.sh` migration.

App versions live under `/opt/punktfunk-releases`; `/opt/punktfunk-rpi5` is an atomic
symlink. Downloads and dependencies are checked before stopping the old app. The
systemd worker survives the app restart, keeps the previous version and restores
it if the new process fails its 20-second startup check. Boot recovery covers an
interrupted activation. This process check does not prove visual/playback quality.

The root-owned sticky `/data` parent preserves app write access while protecting
root-private updater and VirtualHere directories. Pairings, SSH state and licenses
are outside app packages. The manual standalone installer refuses to overwrite a
managed kiosk installation.

## Verification

- Writing, staged whitespace, shell syntax and exact patch replay passed.
- WSL: 62 settings tests, three kiosk screen tests and three core helper tests passed.
  The rendered update page was inspected at 1080p.
- All 27 Python helper tests passed in WSL, on the physical Pi and in the image job.
  They cover checksum mismatch, OS incompatibility, archive traversal, private
  staging permissions, concurrent operations, failed startup and interrupted work.
- Native ARM64: 331 core, 110 video, 507 console, 157 session and three pacing
  tests passed. The suites also reported 28 ignored tests.
- Build 29 stopped because the Bookworm app-build container lacks Python's safe
  tar extraction filter. Kiosk tests run in the image environment instead; the
  kiosk runtime is Trixie and the standalone bundle retains Bookworm compatibility.
- The image passed dependency, service, boot-asset, filesystem and compression
  checks. Workflow 41807087858 first hit HTTP 500 creating the release. The image
  retry finished uploading all assets but returned HTTP 422 for an already present
  checksum asset. The workflow is therefore marked failed at upload, not build.
  All seven published assets were inspected; the downloaded image, inventories,
  build manifest and app archive passed their published SHA-256 checks.

## Physical Pi update

The existing Pi first received .28, then the updater bootstrap over SSH. No disk
was reflashed. The old app directory was migrated into a versioned directory.
Systemd and sudoers validation passed; the app user can write `/data` but cannot
read the updater's private state.

The updater found .30 from the public GitHub release. A controlled test loaded the
installed updater and replaced only its health-check function with a forced
failure. It downloaded and verified the real .30 package, switched versions, then
restored and restarted .28 successfully.

A subsequent normal update ran through `kiosk-control update-install` as the
`punktfunk` service user and the independent systemd worker. It completed with
`Result=success`; the active app is .30 with zero automatic restarts. All six
pre-existing Punktfunk configuration files retained identical hashes. .28 remains
on disk. A follow-up check reports that Punktfunk is up to date.

VirtualHere is available in the menu but its server has not been downloaded or
enabled on this Pi. Physical USB transfer, paid license activation and booting a
freshly flashed .30 image remain unverified.

## Artifacts

Local directory: `D:/Source/Aditi/StreamOS/artifacts/punktfunk-rpi5-30-test`

- Image: `punktfunk-0.46.0-rpi5.30-kiosk.img.xz`, 899344384 bytes.
  SHA-256: `f2e5af060c6f353999d94c1ed83c56e70a027c34ad95c6896a30df14b23bb1b4`.
- App: `punktfunk-0.46.0-rpi5.30-linux-arm64.tar.gz`, 26063486 bytes.
  SHA-256: `f4f7cdc7a0f7203525dea811a754ed6b4ea714c9034af61b30234eff4f0abc11`.
