# StreamOS Raspberry Pi 5 fork

This repository is Aditi's Raspberry Pi 5 client fork of Punktfunk. It preserves
upstream history and is based on upstream's v0.34.0-era `main` at the hardware-tested
commit:

```text
90ce72497f3420f9efcbaaee0eb5fb973ed2bdd2
```

That commit is a descendant of the annotated `v0.34.0` release tag. The fork keeps
the Raspberry Pi changes as four separate commits, in the same order as the former
StreamOS patch series. This makes each downstream concern reviewable and allows an
upstream update to fail visibly at the exact change that needs a deliberate rebase.
The detailed symptom, cause, implementation, classification, and exact commit map
is maintained in [the patch rationale](streamos-rpi5-patch-rationale.md).

## Integrated changes and rationale

1. **Raspberry Pi 5 HEVC hardware decoding.** Adds an FFmpeg V4L2 Request decoder
   selected as `v4l2-request`. Raspberry Pi's HEVC block produces Broadcom SAND
   DMA-BUFs, which V3DV cannot import directly. The matching Raspberry Pi FFmpeg
   build performs an optimized NEON SAND-to-I420 transfer, after which Punktfunk's
   existing planar Vulkan upload path presents the frame. This avoids software HEVC
   decode while retaining the existing Vulkan renderer.

2. **Compositor-paced Vulkan/Wayland presentation.** Uses Wayland compositor frame
   callbacks to prevent the client from outrunning Weston, permits a deeper swapchain
   through `PUNKTFUNK_SWAPCHAIN_IMAGES`, and recreates the swapchain when a suboptimal
   result follows updated Weston DMA-BUF feedback. The intent is stable 60 Hz page
   flips on Pi 5 without stale buffer assumptions after compositor feedback changes.

3. **Damage-bounded overlay rendering.** Restricts the Skia/Vulkan overlay render pass
   to the overlay's actual damage band instead of touching the full output. Full-screen
   overlay work caused the statistics display to reduce observed output from 60 FPS to
   roughly 40 FPS on Pi 5.

4. **Delayed audio reconciliation after PLC.** Tracks timeline positions already
   emitted by Opus packet-loss concealment. A delayed packet for one of those positions
   is neither decoded, queued, nor treated as a new A/V-sync observation. Decoding it
   again would advance decoder and playout state twice, producing audio corruption and
   persistent A/V lag.

The unit tests shipped in the original patches remain next to the implementation,
including decoder selection/fallback, presentation behavior, and delayed-audio
timeline reconciliation coverage.

## Build prerequisites

The Pi client requires an ARM64 Linux userspace with the normal Punktfunk Linux build
dependencies plus:

- Linux media/V4L2 Request support for Raspberry Pi 5 HEVC;
- the Raspberry Pi FFmpeg build that exposes V4L2 Request HEVC decoding and optimized
  Broadcom SAND-to-I420 transfer;
- Vulkan, Wayland, DRM/GBM, SDL3, PipeWire and Opus development files;
- Rust 1.96.0, Clang/libclang, CMake, NASM and `pkg-config`.

The fork enables SDL3's `build-from-source` Cargo feature for Linux. This matches
the prior StreamOS ARM64 builder and removes an otherwise undeclared dependency on
a sufficiently new system SDL3 package. It also disables `pf-presenter` default
features on the console UI's optional presenter edge so the appliance `ui` build
does not pull in the unrelated experimental PyroWave codec stack.

Point `PKG_CONFIG_PATH` and the runtime loader at the Raspberry Pi FFmpeg prefix. For
the StreamOS layout this is:

```sh
export PKG_CONFIG_PATH=/opt/streamos-rpi-ffmpeg/lib/pkgconfig
export LD_LIBRARY_PATH=/opt/streamos-rpi-ffmpeg/lib
export LIBCLANG_PATH=/usr/lib/llvm-14/lib
export PUNKTFUNK_BUILD_VERSION=0.34.0+streamos.90ce72497f34
```

Build the headless launcher and session renderer used by StreamOS:

```sh
cargo build --release -p punktfunk-cli --no-default-features
cargo build --release \
  -p punktfunk-client-session \
  --no-default-features \
  --features ui
```

The resulting programs are `target/release/punktfunk` and
`target/release/punktfunk-session`. Deploy the matching Raspberry Pi FFmpeg shared
libraries with them; using distribution FFmpeg may silently remove the required
decoder or optimized SAND transfer path.

## Verification

The fork integration was checked on 2026-09-03 with:

```sh
cargo test --locked -p pf-client-core -p pf-presenter -p pf-console-ui
```

This passed 277 `pf-client-core`, 64 `pf-presenter`, and 231 `pf-console-ui`
tests (with the existing hardware-dependent ignored tests left ignored), plus their
documentation tests. A native `aarch64-unknown-linux-gnu` release build of those
three affected crates also passed under a `linux/arm64` container using the StreamOS
Raspberry Pi FFmpeg prefix. This records integration coverage, not a substitute for
the physical acceptance run below.

Run the complete Rust test suite after every downstream or upstream change:

```sh
cargo test --workspace
```

Then perform the ARM64 release build above in an actual ARM64 environment or with
Docker Buildx/QEMU using `--platform linux/arm64`. A successful compile is not hardware
validation.

For the physical Raspberry Pi 5 acceptance run:

1. Boot the StreamOS image with the Raspberry Pi media/V4L2 Request kernel support,
   the matching FFmpeg runtime, V3DV Vulkan and Weston active.
2. Stream HEVC at 1920x1080, 60 FPS. Use `PUNKTFUNK_DECODER=v4l2-request` for a forced
   decoder check; also test the normal automatic decoder selection.
3. Confirm logs identify `v4l2-request` / `v4l2-request-planar`, and confirm the first
   decoded frame is used as StreamOS's readiness signal.
4. Run with the statistics overlay off and on. Confirm stable 60 FPS/page-flip cadence
   in both cases and no regression toward the former roughly 40 FPS overlay behavior.
5. Exercise resize/fullscreen or another Weston DMA-BUF feedback change and confirm
   presentation continues after swapchain recreation.
6. Introduce a short network/audio delivery stall and confirm recovery has no duplicated
   audio, corruption, or persistent A/V offset.
7. Verify controller input and the global quit chord return cleanly to StreamOS.

Record the image/build identifier, Pi revision, display mode, decoder log evidence,
frame-rate/page-flip evidence, and pass/fail result. Do not describe an emulated ARM64
build as a physical 1080p60 pass.

## StreamOS integration contract

Some required behavior lives outside this Punktfunk repository. StreamOS must retain
all of the following when consuming the fork:

- PipeWire's real-time data loop with `CAP_SYS_NICE` and `SCHED_FIFO`;
- a stable 48 kHz, 1024-frame PipeWire HDMI quantum;
- Weston compositor-frame pacing support;
- readiness detection from `first frame decoded`, with `{"ready":true}` retained only
  as a legacy fallback;
- the complete Punktfunk session environment and V4L2/FFmpeg runtime libraries;
- controller forwarding; and
- the global quit monitor that terminates the session and restores the launcher.

These items are integration requirements, not claims that this fork configures the
StreamOS services which own them.

## Updating from upstream

Keep `upstream` pointed at the canonical repository:

```sh
git remote add upstream https://git.unom.io/unom/punktfunk.git  # first time only
git fetch upstream --tags
```

Create a temporary rebase branch, move the four downstream commits onto the chosen
upstream commit, and validate before updating `main`:

```sh
git switch -c rebase/upstream-<date> main
git rebase --onto <new-upstream-commit> 90ce72497f3420f9efcbaaee0eb5fb973ed2bdd2
cargo test --workspace
```

For subsequent rebases, replace the old base in that command with the fork's previously
recorded upstream base. Resolve every conflict intentionally and review the resulting
diff. Never use a strategy that silently drops a commit or hunk. Do not update `main`
until tests, the ARM64 release build, and the physical Pi 5 1080p60 checklist pass.

## Upstream and licensing

Upstream Punktfunk: <https://git.unom.io/unom/punktfunk>

The fork remains available under Punktfunk's existing dual MIT OR Apache-2.0 license.
