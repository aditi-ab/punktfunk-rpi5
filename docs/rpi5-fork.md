# Raspberry Pi 5 fork

This standalone fork preserves upstream Punktfunk history and maintains Raspberry
Pi hardware decoding, presentation, Bluetooth, and audio integration. Release tags
identify the upstream version and fork revision. The release's `fork-commit` and
image build report record the exact source; this guide applies across releases.

See [installation](raspberry-pi-5.md) for the complete kiosk image and portable
bundle, and [patch rationale](rpi5-patch-rationale.md) before updating the fork.

## Maintained behavior

- HEVC V4L2 Request decoding uses the matching Raspberry Pi FFmpeg build. NEON
  transfer converts Broadcom SAND surfaces into planar pixels for Vulkan upload.
  Ten-bit decode output is rounded to eight bits; this is not HDR presentation.
- Wayland compositor callbacks pace presentation. Swapchain recreation handles
  updated compositor feedback, and overlay damage limits unnecessary GPU work.
- Audio reconciliation discards delayed packets whose timeline positions already
  played through packet-loss concealment.
- The native console manages Bluetooth devices through BlueZ; see
  [Bluetooth settings](bluetooth-console.md).
- The kiosk starts Punktfunk directly under Weston, with PipeWire audio and a
  software cursor backend that avoids the observed Pi hardware-cursor crash.

## Build the portable bundle

Use the toolchain in `rust-toolchain.toml` and the dependencies in
[the release workflow](../.github/workflows/rpi5-release.yml). The release builder
pins Raspberry Pi FFmpeg and uses the checked-in Cargo lockfile. SDL3 builds from
source and must expose Wayland support before a bundle can pass verification.

On Windows with Docker Desktop, choose a release tag and run:

```powershell
$releaseTag = Read-Host 'Release tag to build'
./packaging/rpi5/build-release-local.ps1 $releaseTag
```

For local changes, add `-WorkingTree`. Tracked changes are overlaid on HEAD and
recorded in `source.patch`; untracked files are excluded. Such a bundle is a local
candidate, not the source for a published image.

On ARM64 Linux with the workflow's dependencies installed:

```sh
read -r -p 'Release tag to build: ' release_tag
bash packaging/rpi5/build-release.sh "$release_tag" dist
```

The builder verifies runtime linkage, SDL Wayland support, and CLI startup, then
writes a portable archive and checksum. For direct developer builds, the session
requires `--no-default-features --features ui,rpi5-v4l2-request` and the Raspberry
Pi FFmpeg development libraries in `PKG_CONFIG_PATH`.

## Build the kiosk image

Check out the release tag and build its portable bundle first. On Linux with root
access and the image job's dependencies installed:

```sh
sudo bash packaging/rpi5/kiosk/build-image.sh "$release_tag" "dist/punktfunk-${release_tag#v}-linux-arm64.tar.gz" dist
```

The image builder verifies the bundle checksum, version, and commit against the
tag. It uses a checksum-pinned Raspberry Pi OS Lite base, installs the matching
bundle and kiosk services, and verifies dependencies and configuration offline.
ARM64 runs natively; other architectures also require `qemu-aarch64-static`.

Output includes the complete `.img.xz`, checksums, package inventory, and build
report. Booting and streaming on a physical Pi remain separate acceptance checks.

## Update and publish

1. Fetch the canonical upstream repository at
   `https://git.unom.io/unom/punktfunk.git` and select a named release tag.
2. Create an integration branch from the current fork tip. Merge the selected
   upstream tag, resolving each conflict against the patch rationale. Preserve
   equivalent upstream improvements and check every downstream feature boundary.
3. Review the diff against both the old fork and new upstream tag. Run the client
   tests with the Pi feature enabled, the ARM64 bundle build, and the kiosk build.
4. Create a new annotated fork tag shaped as `vX.Y.Z-rpi5.N`. Never move a
   published release tag. Push the branch and tag after review.
5. The Raspberry Pi release workflow builds both artifacts from that exact tag.
   Manual dispatch accepts an existing tag. Assets are uploaded together to the
   matching GitHub release; a new release starts as a draft.
6. Download and verify the published checksums, perform physical acceptance, and
   publish the draft with an accurate record of passed and untested checks.

## Physical acceptance

Use the image and portable bundle identifiers to record what was tested:

1. Boot on Pi 5 and confirm that Punktfunk opens directly, discovers the host, and
   retains pairing/settings across a restart.
2. Stream HEVC at 1920x1080, 60 FPS. Confirm logs report `v4l2-request` and the
   first decoded frame; internal FPS alone is not proof of physical cadence.
3. Compare visible motion and page-flip cadence with the statistics overlay off
   and on. Exercise resize or compositor feedback changes.
4. Scroll the home screen with a controller for a sustained interval. Confirm
   smooth motion and no Weston restart.
5. Test delayed audio recovery, controller input, return to the home screen,
   Bluetooth pairing/cancel/reconnect, and a reboot.

Record the Pi revision, display mode, codec, image/tag, commit, and log evidence.
Earlier Pi testing established stable 60 FPS streaming and smooth home-screen
scrolling after the cursor fix; each new release still needs its own hardware run.

## Licensing

Upstream Punktfunk is available under MIT OR Apache-2.0. Bundled third-party
components retain their own licenses and notices.
