# StreamOS Raspberry Pi 5 patch rationale

This document records the downstream delta carried by the Aditi Raspberry Pi 5
fork, the failure that motivated each runtime patch, and whether the change is
Pi-specific or a candidate for upstream Punktfunk. Read it before rebasing the
fork, changing the Raspberry Pi release build, or proposing a fix upstream.

The fork base for release `v0.34.0-rpi5.3` is upstream commit
[`90ce72497f3420f9efcbaaee0eb5fb973ed2bdd2`](https://git.unom.io/unom/punktfunk/commit/90ce72497f3420f9efcbaaee0eb5fb973ed2bdd2).
The four original StreamOS patches are preserved as individual Git commits. Git
history is authoritative; use `git diff 90ce7249..v0.34.0-rpi5.3` to inspect the
complete release delta.

## Runtime patch map

| Original patch | Fork commit | Classification | Keep for a non-Pi fork? |
| --- | --- | --- | --- |
| `0001-client-add-Raspberry-Pi-V4L2-request-decoder.patch` | [`64b7427b`](https://github.com/aditi-ab/punktfunk-rpi5/commit/64b7427bc98af7a45c6f3f5c3ae26add8f9e1711) | Raspberry Pi 5 hardware enablement | Only with the Pi V4L2 Request decoder and matching FFmpeg |
| `0002-presenter-pace-Wayland-frames-from-compositor-callbacks.patch` | [`d80173fe`](https://github.com/aditi-ab/punktfunk-rpi5/commit/d80173fe7d592ff99dd58699e3eff239316fbb97) | General Wayland presentation correctness, exposed on Pi/Weston | Yes, for affected Wayland compositors |
| `0003-presenter-restrict-overlay-rendering-to-its-damage-band.patch` | [`8257c318`](https://github.com/aditi-ab/punktfunk-rpi5/commit/8257c318a9b591390725360614973b6f8ea857f8) | General Vulkan overlay optimization with a large Pi impact | Yes, where full-surface blending is costly |
| `0004-audio-do-not-replay-delayed-packets-after-PLC.patch` | [`e0326c24`](https://github.com/aditi-ab/punktfunk-rpi5/commit/e0326c247a8cedeb53c6e1828b9c80fb8caf0ac6) | General client audio timeline correctness | Yes |

Only the first patch is inherently Raspberry Pi-specific. The other three are
plausible upstream fixes or optimizations, although this fork validates them on
the Pi 5, Weston, and StreamOS stack.

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
[`64b7427b`](https://github.com/aditi-ab/punktfunk-rpi5/commit/64b7427bc98af7a45c6f3f5c3ae26add8f9e1711):

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
[`84f2ade9`](https://github.com/aditi-ab/punktfunk-rpi5/commit/84f2ade9df9786280b520cfc53d0b4fc9e251a38)
adds compatibility with the older FFmpeg headers used by the Raspberry Pi build,
and [`d2114543`](https://github.com/aditi-ab/punktfunk-rpi5/commit/d2114543912e2919c1ec3e0cfc239a8508ff20d8)
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

Commit [`d80173fe`](https://github.com/aditi-ab/punktfunk-rpi5/commit/d80173fe7d592ff99dd58699e3eff239316fbb97):

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

Commit [`8257c318`](https://github.com/aditi-ab/punktfunk-rpi5/commit/8257c318a9b591390725360614973b6f8ea857f8)
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

Commit [`e0326c24`](https://github.com/aditi-ab/punktfunk-rpi5/commit/e0326c247a8cedeb53c6e1828b9c80fb8caf0ac6)
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

Commit [`a360a44e`](https://github.com/aditi-ab/punktfunk-rpi5/commit/a360a44e841a40450cdf1c9b5e38afaf0f4f84a3)
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

## Supporting fork changes in `v0.34.0-rpi5.3`

The following commits are part of the release delta but are not additional
runtime bug patches:

| Commit | Purpose |
| --- | --- |
| [`2df1e5d8`](https://github.com/aditi-ab/punktfunk-rpi5/commit/2df1e5d84cc40fa829e90ffa6884fae2137a9967) | Formats the imported patch series without changing intent. |
| [`9c88dba3`](https://github.com/aditi-ab/punktfunk-rpi5/commit/9c88dba35e85ffe6e2b9302ae9ae35e4d00cae63) | Builds SDL3 from source for appliance targets. |
| [`c508e1db`](https://github.com/aditi-ab/punktfunk-rpi5/commit/c508e1db5aadf37fd4d8a0e698a8aaea05776d01) | Keeps the unrelated experimental PyroWave feature out of the console UI edge. |
| [`f08f6615`](https://github.com/aditi-ab/punktfunk-rpi5/commit/f08f66158d4d163145d9b0b652d8bc1939c8571f) | Adds the standalone ARM64 release bundle, installer, and CI workflow. |
| [`c3b3bb37`](https://github.com/aditi-ab/punktfunk-rpi5/commit/c3b3bb373170135937d26739131a878e4e1f3758) | Builds releases on a Raspberry Pi OS-compatible baseline. |
| [`dc03dd66`](https://github.com/aditi-ab/punktfunk-rpi5/commit/dc03dd66c09cca22fcbddb5bc970275f2a0891be) | Marks the container workspace safe for the release build. |
| [`efc5db21`](https://github.com/aditi-ab/punktfunk-rpi5/commit/efc5db21f5d51a210106d6d7113dd47286ca63d7) | Preserves the upstream locked Android resolution. |
| [`605d4a66`](https://github.com/aditi-ab/punktfunk-rpi5/commit/605d4a667cf2748b62cef114125db3e549c1f48d) | Builds the committed workspace without release-time manifest rewriting. |
| [`d078c848`](https://github.com/aditi-ab/punktfunk-rpi5/commit/d078c8482d88e2917129aa98d9f27fa1063b7d37) | Bundles the SDL3 runtime required by the standalone binaries. |
| [`06445740`](https://github.com/aditi-ab/punktfunk-rpi5/commit/0644574049680b0d9e37e8b6d137f4119bdf4b9c) | Installs the release uploader in CI. |
| [`a360a44e`](https://github.com/aditi-ab/punktfunk-rpi5/commit/a360a44e841a40450cdf1c9b5e38afaf0f4f84a3) | Requires Wayland support in the SDL3 runtime and verifies it before publishing. |
| [`3477efa2`](https://github.com/aditi-ab/punktfunk-rpi5/commit/3477efa2) | Restores SDL's Wayland prerequisites and adds the local ARM64 release preflight. |
| [`823e072d`](https://github.com/aditi-ab/punktfunk-rpi5/commit/823e072d) | Dereferences annotated tags when normalizing archive timestamps. |
| [`2a8c48c4`](https://github.com/aditi-ab/punktfunk-rpi5/commit/2a8c48c4) | Keeps local release output outside version control. |

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
