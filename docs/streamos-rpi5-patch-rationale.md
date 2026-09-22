# StreamOS Raspberry Pi 5 patch rationale

This document records the downstream delta carried by the Aditi Raspberry Pi 5
fork, the failure that motivated each runtime patch, and whether the change is
Pi-specific or a candidate for upstream Punktfunk. Read it before rebasing the
fork, changing the Raspberry Pi release build, or proposing a fix upstream.

The fork base for release `v0.39.0-rpi5.1` is upstream commit
[`b1051a3a1cc7a04a1ae3a11691b32dc851a6f8a9`](https://git.unom.io/unom/punktfunk/commit/b1051a3a1cc7a04a1ae3a11691b32dc851a6f8a9).
The four original StreamOS patches are preserved as individual Git commits. Git
history is authoritative; use `git diff b1051a3a..v0.39.0-rpi5.1` to inspect the
complete release delta.

## Runtime patch map

| Original patch | Fork commit | Classification | Keep for a non-Pi fork? |
| --- | --- | --- | --- |
| `0001-client-add-Raspberry-Pi-V4L2-request-decoder.patch` | [`ccbc34b7`](https://github.com/aditi-ab/punktfunk-rpi5/commit/ccbc34b776c8d803c4cbc6f42f212ce39c1afbbc) | Raspberry Pi 5 hardware enablement | Only with the Pi V4L2 Request decoder and matching FFmpeg |
| `0002-presenter-pace-Wayland-frames-from-compositor-callbacks.patch` | [`00920937`](https://github.com/aditi-ab/punktfunk-rpi5/commit/00920937930f4b1a608c26cd0c4933d32177dc3c) | General Wayland presentation correctness, exposed on Pi/Weston | Yes, for affected Wayland compositors |
| `0003-presenter-restrict-overlay-rendering-to-its-damage-band.patch` | [`012df8d7`](https://github.com/aditi-ab/punktfunk-rpi5/commit/012df8d710c473645951893bd59170e36e428ab9) | General Vulkan overlay optimization with a large Pi impact | Yes, where full-surface blending is costly |
| `0004-audio-do-not-replay-delayed-packets-after-PLC.patch` | [`9626fc01`](https://github.com/aditi-ab/punktfunk-rpi5/commit/9626fc01f93fae6002f6de0bd9babe3f41140109) | General client audio timeline correctness | Yes |

Only the first patch is inherently Raspberry Pi-specific. The other three are
plausible upstream fixes or optimizations, although this fork validates them on
the Pi 5, Weston, and StreamOS stack.

## v0.39.0 rebase decision

Upstream v0.39.0 removed FFmpeg from the normal client dependency graph and split
desktop and PyroWave capabilities into explicit Cargo features. The Pi decoder
still requires the Raspberry Pi FFmpeg fork, so release `v0.39.0-rpi5.1` exposes
it through the explicit `rpi5-v4l2-request` feature. That feature enables the
desktop client path and `ffmpeg-sys-next`; ordinary upstream builds remain
FFmpeg-free. The StreamOS session build must use
`--features ui,rpi5-v4l2-request`.

The rebase conflicts were resolved at the decoder module boundary, decoded-image
dispatch, presenter dependency features, session feature forwarding, and release
builder. These are the areas to inspect first during the next upstream update.
The release builder uses `--locked`, so any feature dependency change must also
be reflected in `Cargo.lock` before creating a candidate tag.

## 0001: Raspberry Pi V4L2 Request HEVC decoding

### Observed failure

Stock Punktfunk could not use the Raspberry Pi 5 HEVC hardware decoder. The Pi
kernel exposes a stateless V4L2 Request decoder rather than the stateful V4L2 M2M
or VA-API interfaces supported by the normal Linux decoder ladder. Punktfunk
therefore reported `software`. Physical 1080p60 tests produced roughly 40--56 FPS
and decode times ranging from tens to hundreds of milliseconds. Lowering bitrate
made the stream blurry without fixing the absent hardware path.

### Cause and implementation

The hardware decoder returns DRM PRIME frames using Broadcom's SAND layout. V3DV
cannot import that modifier directly into Punktfunk's Vulkan presenter. Commit
[`ccbc34b7`](https://github.com/aditi-ab/punktfunk-rpi5/commit/ccbc34b776c8d803c4cbc6f42f212ce39c1afbbc):

- adds a Linux decoder rung selected by `PUNKTFUNK_DECODER=v4l2-request`;
- opens FFmpeg's HEVC V4L2 Request decoder through `ffmpeg-sys-next`;
- receives DRM PRIME/SAND hardware frames;
- uses the Raspberry Pi FFmpeg fork's optimized NEON transfer to detile SAND to
  planar I420;
- feeds I420 into Punktfunk's existing Vulkan upload and color-conversion path;
  and
- reports `v4l2-request` / `v4l2-request-planar` as runtime evidence.

HEVC bitstream decoding remains in the Pi hardware block. The CPU performs the
required SAND-to-I420 transfer and upload; this must not be described as software
HEVC decoding. The rung intentionally rejects non-HEVC streams.

Primary implementation files are
[`video_v4l2_request.rs`](../crates/pf-client-core/src/video_v4l2_request.rs),
[`video.rs`](../crates/pf-client-core/src/video.rs), and
[`session.rs`](../crates/pf-client-core/src/session.rs). Commit
[`a43c1327`](https://github.com/aditi-ab/punktfunk-rpi5/commit/a43c1327)
adds compatibility with the older FFmpeg headers used by the Raspberry Pi build,
and [`7ff28efa`](https://github.com/aditi-ab/punktfunk-rpi5/commit/7ff28efa)
locks the client FFmpeg dependency.

Expected log evidence includes:

```text
Raspberry Pi V4L2 Request HEVC hardware decode active (NEON SAND transfer)
decode rung active rung="v4l2-request"
first frame decoded ... path="v4l2-request-planar"
```

This patch depends on the matching Raspberry Pi FFmpeg build. Removing or
changing that dependency must make the rung unavailable rather than silently
labelling software decode as hardware.

## 0002: compositor-paced Wayland presentation

### Observed failure

Punktfunk could report 60 decoded and rendered FPS while physical motion looked
closer to 30 or 40 FPS. A session could begin smoothly and settle into an uneven
cadence. FIFO could halve effective compositor cadence, while MAILBOX could settle
into an uneven pattern. Network and decoder statistics remained healthy.

Weston could also update DMA-BUF feedback after swapchain creation. Ignoring a
successful-but-suboptimal acquire or present result kept stale swapchain choices.

### Cause and implementation

Vulkan Wayland WSI present modes did not expose a reliable physical repaint clock
on this path, and `VK_KHR_present_wait` was unavailable. The client could submit
independently of Weston's repaint opportunity, so internal FPS counters did not
measure evenly latched KMS frames.

Commit [`00920937`](https://github.com/aditi-ab/punktfunk-rpi5/commit/00920937930f4b1a608c26cd0c4933d32177dc3c):

- obtains the native `wl_surface` from SDL;
- attaches a one-shot `wl_surface.frame` callback to each surface commit;
- waits for the compositor callback before the next present and wakes the SDL
  event loop when it completes;
- leaves non-Wayland and Vulkan present-wait paths unchanged;
- provides `PUNKTFUNK_WAYLAND_FRAME_PACING=0` as a diagnostic opt-out;
- provides `PUNKTFUNK_SWAPCHAIN_IMAGES` for compositors retaining several
  buffers; and
- recreates the swapchain after suboptimal acquire/present results.

The implementation is in
[`wayland_frame.rs`](../crates/pf-presenter/src/vk/wayland_frame.rs) and the
adjacent Vulkan presenter modules. It contains no 60 Hz timer: compositor
callbacks follow the active output and therefore apply to 50, 60, 120 Hz, and
other supported modes.

Expected logs include `Wayland compositor frame pacing active` and, with
presentation debugging enabled, `Wayland frame pacing window ...`.

## 0003: damage-bounded overlay rendering

### Observed failure

Enabling Punktfunk's statistics overlay reduced a stream that was otherwise near
60 physical page flips per second to roughly 40 on Pi 5. Decode and network
numbers remained healthy while display cost and visible judder increased.

### Cause and implementation

The Skia/Vulkan overlay pass blended the complete 1920x1080 surface even when its
visible content occupied only a narrow top or bottom band. That full-surface
read/modify/write consumed enough memory bandwidth and GPU time to disturb scanout
cadence.

Commit [`012df8d7`](https://github.com/aditi-ab/punktfunk-rpi5/commit/012df8d710c473645951893bd59170e36e428ab9)
carries `scissor_y` and `scissor_height` from
[`skia_overlay.rs`](../crates/pf-console-ui/src/skia_overlay.rs) through the
overlay frame into the Vulkan render pass. Normal top and bottom chrome use a
bounded band. Resize scrims, the quick ring, and simultaneous top-and-bottom
chrome retain a full-surface pass so content is not clipped.

This changes overlay drawing cost only. It does not change stream bitrate,
decoding, capture cadence, or display mode.

## 0004: delayed audio after packet-loss concealment

### Observed failure

During sustained streaming, audio could drift out of sync and eventually become
intermittent, corrupt, or silent after a delivery stall. The symptom could take
minutes to appear and was distinct from HDMI/PipeWire scheduling failures.

### Cause and implementation

During an audio drought, Punktfunk requested Opus packet-loss concealment (PLC)
frames and advanced the decoder/playout timeline. If the original packets were
delayed rather than lost, the receive path could later decode and queue the same
timeline positions, advancing decoder and playout state twice. A delayed packet
could also become an invalid A/V-sync observation.

Commit [`9626fc01`](https://github.com/aditi-ab/punktfunk-rpi5/commit/9626fc01f93fae6002f6de0bd9babe3f41140109)
tracks positions already covered by drought PLC. Sequence gaps consume those
positions first. A packet already represented by PLC is not decoded, queued, or
submitted to A/V synchronization; genuinely missing positions beyond the
concealed span still receive normal PLC.

The focused tests in
[`session.rs`](../crates/pf-client-core/src/session.rs) are:

- `delayed_audio_covered_by_drought_plc_is_not_queued_twice`; and
- `sequence_gaps_consume_concealed_timeline_before_new_plc`.

This is a general Punktfunk correctness fix and a strong candidate for an
upstream issue and pull request. It does not claim to fix every form of HDMI
silence.

## Release SDL Wayland capability

Release `v0.34.0-rpi5.1` built SDL3 from source without all of SDL's Wayland
build prerequisites. In particular, the container lacked the `egl` pkg-config
metadata supplied by `libegl1-mesa-dev`. The resulting standalone bundle passed
linkage checks but SDL exposed no Wayland video driver, so StreamOS launch failed
immediately with `presenter: SDL video: wayland not available` and exit code 4.

Commit [`2a8474e1`](https://github.com/aditi-ab/punktfunk-rpi5/commit/2a8474e1557e46643716e4695a63f1b332927439)
installs `wayland-protocols` in the ARM64 release job. The bundle builder now asks
SDL for its compiled video drivers and rejects the artifact unless `wayland` is
present. That guard intentionally rejected the `v0.34.0-rpi5.2` build and kept
the invalid binary out of the release assets. The next release also installs
Mesa's EGL, OpenGL, and OpenGL ES development packages, matching SDL's documented
Linux video prerequisites.

Before a release tag is pushed, `packaging/rpi5/build-release-local.ps1` builds
the same Debian Bookworm toolchain container for ARM64 with Docker and runs the
normal bundle builder. The bundle's SDL driver check and linkage checks therefore
run locally against the exact archive that will be published. This is a
release-packaging fix; it does not change Punktfunk runtime behavior or the four
downstream source patches.

The same preflight exposed an annotated-tag timestamp bug in the archive step.
The builder now dereferences the tag to its commit before reading the commit
timestamp, so every archive member receives a valid, reproducible modification
time.

## Supporting fork changes retained in `v0.39.0-rpi5.1`

The following commits are part of the release delta but are not additional
runtime bug patches:

| Commit | Purpose |
| --- | --- |
| [`32def45f`](https://github.com/aditi-ab/punktfunk-rpi5/commit/32def45fec4d34e73646544d043c874b55e915c3) | Formats the imported patch series without changing intent. |
| [`13a39869`](https://github.com/aditi-ab/punktfunk-rpi5/commit/13a3986999f14994911f3c33bcab70b39c0ed7d3) | Builds SDL3 from source for appliance targets. |
| [`2c7a119b`](https://github.com/aditi-ab/punktfunk-rpi5/commit/2c7a119b4791ebe0ee23fdc5a42536988ff9d93b) | Keeps the unrelated experimental PyroWave feature out of the console UI edge. |
| [`c52975af`](https://github.com/aditi-ab/punktfunk-rpi5/commit/c52975affd8c226ce146cc25782edc30c4e333af) | Adds the standalone ARM64 release bundle, installer, and CI workflow. |
| [`262a14a2`](https://github.com/aditi-ab/punktfunk-rpi5/commit/262a14a2dab476c423cfce6d1cc4c5a1267b1840) | Builds releases on a Raspberry Pi OS-compatible baseline. |
| [`ac93b616`](https://github.com/aditi-ab/punktfunk-rpi5/commit/ac93b6168bf21a4dc5af1a4367b8f3430af020f5) | Marks the container workspace safe for the release build. |
| [`e7538417`](https://github.com/aditi-ab/punktfunk-rpi5/commit/e7538417f03c5a940cc58bde67cbd44fd22f4e72) | Preserves the upstream locked Android resolution. |
| [`943f9ec0`](https://github.com/aditi-ab/punktfunk-rpi5/commit/943f9ec01dd389c378bae74751a4f64aa302a1b1) | Builds the committed workspace without release-time manifest rewriting. |
| [`3fbabf0b`](https://github.com/aditi-ab/punktfunk-rpi5/commit/3fbabf0bef4dfbb1a3c1cdb6246e386ade65d645) | Bundles the SDL3 runtime required by the standalone binaries. |
| [`13eb2178`](https://github.com/aditi-ab/punktfunk-rpi5/commit/13eb217862cae1e974e11d1c8b86a3ab15e6caf8) | Installs the release uploader in CI. |
| [`2a8474e1`](https://github.com/aditi-ab/punktfunk-rpi5/commit/2a8474e1557e46643716e4695a63f1b332927439) | Requires Wayland support in the SDL3 runtime and verifies it before publishing. |
| [`373cc4a8`](https://github.com/aditi-ab/punktfunk-rpi5/commit/373cc4a82c4e9006b2e080bbe194a9d69cd172f8) | Restores SDL's Wayland prerequisites and adds the local ARM64 release preflight. |
| [`c3107b0e`](https://github.com/aditi-ab/punktfunk-rpi5/commit/c3107b0eaa2c4d972e908899c68ca6b6dbec1359) | Dereferences annotated tags when normalizing archive timestamps. |
| [`c86ad6c4`](https://github.com/aditi-ab/punktfunk-rpi5/commit/c86ad6c423c5aa99a0925f855c9730032d1545d7) | Keeps local release output outside version control. |
| [`e3172346`](https://github.com/aditi-ab/punktfunk-rpi5/commit/e31723465f14cd9aa0974633945d96815a00d374) | Gates the Pi decoder and FFmpeg dependency behind `rpi5-v4l2-request`. |
| [`d14a93da`](https://github.com/aditi-ab/punktfunk-rpi5/commit/d14a93dadbb400df98c70834f79601d04c700860) | Locks the optional dependency and enforces Linux line endings in release scripts. |

Documentation and repository-identification commits are intentionally omitted
from that implementation table but remain visible in the base-to-tag Git log.

## Changes that remain in StreamOS

The following related behavior belongs to the appliance integration and must not
be presented as Punktfunk source fixes:

- PipeWire realtime privileges, graph rate/quantum, HDMI ownership, and service
  restart policy;
- the opaque loading surface, first-frame readiness detection, wake/launch
  timeouts, cancellation, and suppression of expected SIGTERM status 143;
- resolution, refresh, codec, bitrate, HDR, presentation, and overlay policy;
- host discovery, wake-on-LAN metadata, pairing persistence, and launcher UI;
- the matching Raspberry Pi FFmpeg build and its installation path; and
- controller forwarding and the global StreamOS quit monitor.

These requirements are summarized in
[`streamos-rpi5.md`](streamos-rpi5.md). StreamOS consumes the fork but remains
responsible for its own session supervision and OS services.

## Rebase and validation checklist

1. Record the old and proposed upstream base commits.
2. Rebase each downstream commit deliberately; do not silently drop a commit or
   hunk to make the rebase pass.
3. Review `git range-diff <old-base>..<old-tip> <new-base>..<new-tip>` and update
   this rationale if implementation or behavior changed.
4. Keep the V4L2 Request rung and the pinned Raspberry Pi FFmpeg build compatible.
5. Run the Rust tests, including the PLC tests, and produce the ARM64 release
   bundle.
6. On Pi 5, verify 1920x1080 HEVC reports `v4l2-request`, reaches the first-frame
   readiness event, and does not fall back to `software`.
7. Verify physical cadence with statistics both off and on at 60 Hz and another
   supported mode such as 50 or 120 Hz.
8. Exercise compositor resize or mode changes and confirm swapchain recreation.
9. Run a sustained audio test including delayed or stalled delivery. Diagnose
   StreamOS PipeWire/HDMI health separately from Punktfunk packet reconciliation.
10. Record the upstream base, fork commit/tag, Pi revision, image identifier,
    display mode, codec, and relevant log evidence with the release.

Temporary experiments that did not form part of the validated solution—such as
disabling the overlay entirely or adding a separate source-clock pacing branch—
are intentionally absent and should not be restored merely to ease a rebase.
