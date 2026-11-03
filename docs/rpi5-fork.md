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
  The kiosk selects this Pi-tested path explicitly; upstream's native V4L2
  backend remains available without replacing it.
  Synchronous decoding remains the default. An opt-in two-thread diagnostic can
  overlap hardware work with pixel transfer; outputs retain their input timestamps
  and recovery flags across buffered frames.
- Wayland compositor callbacks pace presentation. Swapchain recreation handles
  updated compositor feedback, and overlay damage limits unnecessary GPU work.
  The kiosk permits two pending callbacks and uses arrival-driven latency mode,
  avoiding a second pacing clock on top of the compositor.
  Animated menus use native refresh deadlines that include rendering time;
  early input wakes do not submit extra frames, and idle frames keep their wait.
  Page transitions keep title and hint fades within their screen bands. Spring
  animation time follows elapsed time even when rendering falls below 20 FPS.
  Efficient eight-bit transitions bound content layers to their recorded drawing,
  including shadows, while retaining native-resolution text and controls.
- Audio reconciliation discards delayed packets whose timeline positions already
  played through packet-loss concealment.
- The native console manages Bluetooth devices through BlueZ; see
  [Bluetooth settings](bluetooth-console.md).
- The kiosk starts Punktfunk directly under Weston, with PipeWire audio and a
  software cursor backend that avoids the observed Pi hardware-cursor crash.
  Weston may use hardware display planes; software cursors do not require
  forcing every fullscreen frame through compositor rendering.
- The kiosk uses an eight-bit SDR swapchain. Broadcom menus omit progressive
  backdrop blurs and scroll fade layers while retaining native-resolution text
  and controls. Fractional video scaling uses bilinear sampling on Broadcom,
  avoiding the floating-point intermediate and high-order filter passes.

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

## Repeatable UI diagnostics

Copy `tools/rpi5-ui-probe.py` to the Pi and run it through an authenticated SSH
session with `sudo python3`. It creates a temporary keyboard through `/dev/uinput`
and removes it when the command ends. No daemon or additional network listener
is installed.

```sh
sudo python3 rpi5-ui-probe.py shortcut disconnect
sudo python3 rpi5-ui-probe.py tap down --count 2
sudo python3 rpi5-ui-probe.py scroll --seconds 20
sudo python3 rpi5-ui-probe.py tabs --seconds 20
sudo python3 rpi5-ui-probe.py shortcut stats
```

Run navigation commands only after confirming the kiosk view: during streaming,
ordinary keys go to the host. `disconnect`, `stats`, and `ring` use Punktfunk's
existing Ctrl+Alt+Shift shortcuts. `--dry-run` prints the requested action without
creating a device. Scroll tests alternate up/down every three seconds.
`tap pageup` and `tap pagedown` switch the console's tabs.
`tabs` alternates those keys to measure page transitions separately from scrolling.

For repeatable cadence measurements, place `tools/rpi5-frame-check.py` beside the
probe on the Pi. After confirming that the console menu is open, run:

```sh
sudo python3 rpi5-frame-check.py menu.trace --capture --tabs --seconds 20
python3 rpi5-frame-check.py menu.trace --min-transition-fps 30
```

Capture uses a private tracing instance and removes it afterwards. Output paths
must be new. Omit `--tabs` for passive delivery measurements without input.
The JSON separates whole-capture cadence from the first 300 ms of each tab switch,
with frame intervals and transition stalls. Select `--crtc` for multiple displays.
Overflow, interrupted captures and partial transitions fail instead of producing
a passing result. Set thresholds from a matching display mode and software build.
DRM deliveries measure display cadence, not proof that each frame has new content.
The optional threshold exits nonzero for low cadence or absent transition markers.

Run parser regression tests with
`python3 -m unittest discover -s tools -p test_rpi5_frame_check.py -v`.

For Wayland pacing comparisons, `PUNKTFUNK_WAYLAND_FRAME_DEPTH=1|2` selects the
pending callback limit. The kiosk uses two; the generic presenter defaults to one.
On Broadcom GPUs the animated background uses a 192-pixel shader target long edge;
text, controls and transition layers retain the native viewport. For comparisons,
`PUNKTFUNK_RPI_FIELD_EDGE=96` selects a smaller background target. Diagnostic
values from 64 through 192 are accepted; invalid values use the default.
Opaque SDR menus on Broadcom use an image transfer when source and output formats
match. The producer must advertise transfer-source usage and a complete native
frame. HDR, partial overlays and format changes retain the graphics pass.
`PUNKTFUNK_RPI_MENU_COPY=0` selects the graphics pass for comparison.
Broadcom SDR tab changes rasterize each page once, then slide and fade those native
images. Page content resumes live rendering when the transition settles; input,
screen state, tabs and pinned controls continue updating throughout. New tab mounts
and geometry changes discard the images. Navigation within a tab retains live rendering.
`PUNKTFUNK_RPI_TAB_IMAGES=0` selects live page rendering during tab changes.
`PUNKTFUNK_RPI_DIRECT_TRANSFER=0` compares against the original planar-copy path.
`PUNKTFUNK_RPI_DECODE_THREADS=2` tests overlapping decode and transfer. This can
increase memory contention and reduce presentation throughput at high output
resolutions, so the default remains one thread.
The decoder's DEBUG timing separates codec work, SAND transfer and pixel copying
from the pre-decode queue delay included in streaming statistics.

For captures, Weston provides `weston-screenshooter` when started with `--debug`.
Enable that only for a diagnostic session, run the capture as the kiosk user with
its Wayland environment, and remove the temporary compositor override afterwards.

## Update and publish

The complete downstream delta lives separately on this repository's
[`codex/rpi5-patches`](https://github.com/aditi-ab/punktfunk-rpi5/tree/codex/rpi5-patches)
maintenance branch, checked out in its own directory. Do not merge that branch into
the source branch. Upstream release tags remain unchanged.
Its immutable snapshots contain ordered binary-safe patches, upstream/source
commits, checksums and an exact-tree replay check. This includes source fixes,
Bluetooth/SSH/system menus, kiosk services, Weston integration and release tooling.

After committing and pushing a fork change, run from the separate patch checkout:

```sh
python3 tools/patchset.py export --source /path/to/punktfunk-rpi5 --base "$upstream_tag"
python3 tools/patchset.py verify --source /path/to/punktfunk-rpi5 --expect-head
```

Commit and push the patch checkout before tagging the fork release. Release builds
fail when their exact source snapshot is absent or cannot recreate the tagged tree.
On a new upstream base, follow the patch branch's porting procedure; an old
patch applying cleanly does not replace compatibility and hardware tests.

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
