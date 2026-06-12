---
title: "Linux Host Setup"
description: "Bring up the build environment on an NVIDIA-GPU Ubuntu VM."
---


How to bring up the build environment for the punktfunk Linux host on an NVIDIA-GPU Ubuntu VM
and run the **M0** capture→encode spike. `punktfunk-core` already builds and is tested
cross-platform; this is about the platform backends in `crates/punktfunk-host`.

> Target **Ubuntu 24.04 (noble)**: Sway 1.9, FFmpeg 6.1.1, xdg-desktop-portal 1.18.
> 22.04 (jammy) ships Sway 1.7 / FFmpeg 4.4 — too old for this path; build from source or
> upgrade. Package names/versions below were verified against the live Ubuntu archive.

## 1. Bootstrap

```sh
git clone git@git.unom.io:unom/punktfunk.git && cd punktfunk && git checkout m1-punktfunk-core
bash scripts/bootstrap-ubuntu.sh
```

It **verifies** the (already-installed) NVIDIA + NVENC stack, installs the Rust toolchain
(rustup) and the build/runtime deps (PipeWire, xdg-desktop-portal + the wlroots backend,
Sway, Wayland/DRM/EGL/GBM/VA dev libs, capture tools), **gates** the FFmpeg `-dev`
headers so it can't clobber your custom NVENC FFmpeg, and drops headless-Sway + portal
config templates into `~/.config` (only if absent). It does **not** reboot or edit GRUB.

After it runs, sanity-check the core on Linux:

```sh
cargo test --workspace        # 21 tests; same suite that's green on macOS
```

## 2. NVIDIA prerequisites (one-time, may need a reboot)

Wayland on NVIDIA requires KMS modeset. The bootstrap checks it; if it isn't `Y`:

```sh
echo 'options nvidia-drm modeset=1 fbdev=1' | sudo tee /etc/modprobe.d/nvidia-drm.conf
sudo update-initramfs -u && sudo reboot
cat /sys/module/nvidia_drm/parameters/modeset   # must print Y after reboot
```

- Driver **≥ 535** is the floor for headless wlroots (EGL/dmabuf); 550+ recommended.
- **Install the NVIDIA GL/EGL userspace, not just `nvidia-utils`:**
  `sudo apt install libnvidia-gl-<NNN>` (matching the driver, e.g. `libnvidia-gl-595`).
  `nvidia-utils-NNN` ships nvidia-smi + NVENC but **not** `libEGL_nvidia.so.0` or the GLVND
  vendor JSON (`/usr/share/glvnd/egl_vendor.d/10_nvidia.json`). Without them libglvnd falls
  back to Mesa, wlroots can't init EGL on the GPU and drops to the **pixman** software
  renderer — and the ScreenCast portal then fails to negotiate a buffer format
  (`unable to receive a valid format from wlr_screencopy`). Verify after install:
  `ls /usr/share/glvnd/egl_vendor.d/10_nvidia.json && ldconfig -p | grep libEGL_nvidia`.
  A correct GPU Sway logs `EGL vendor: NVIDIA` and a list of DMA-BUF formats.
- **Join the `render` + `video` groups:** `sudo usermod -aG render,video $USER`, then
  **re-login** (group changes only apply to new logins). wlroots opens
  `/dev/dri/renderD128` (group `render`) and `/dev/dri/card*` (group `video`), both 0660;
  without membership Sway aborts with `Permission denied`. (`scripts/headless/*.sh` bridge a
  not-yet-re-logged-in shell with `sg render`, but re-login is the clean fix.)
- A **headless VM GPU exposes no DRM connectors** — that's expected. We don't use the DRM
  backend; `WLR_BACKENDS=headless` renders to an offscreen GBM/EGL surface and creates a
  virtual `HEADLESS-1` output. Use the render node `/dev/dri/renderD128`.
- **NVENC in a VM:** full PCI **passthrough** = bare-metal NVENC, no license. **vGPU**
  needs a valid license (vWS) or NVENC runs degraded — the bootstrap's smoke-encode tells
  you if it actually works. Consumer GeForce cards also cap concurrent NVENC sessions
  (~8); datacenter/RTX-pro are effectively unlimited — relevant once we serve many clients.

## 3. Bring up the headless compositor + prove capture→NVENC

```sh
# shell 1 — start headless GPU Sway on the shared user bus (blocks; -d for debug log)
bash scripts/headless/run-headless-sway.sh        # success logs "EGL vendor: NVIDIA"

# shell 2 — same user: set the client mode, import the portal env, write the env file
bash scripts/headless/prepare-session.sh 2560x1440@60Hz
source /tmp/punktfunk-sway-env.sh
swaymsg -t get_outputs                       # confirm HEADLESS-1 active
swaymsg exec foot                            # optional: animated content to capture
bash scripts/headless/capture-smoke-test.sh  # wf-recorder (wlr-screencopy) -> hevc_nvenc
ffprobe /tmp/punktfunk-headless-test.mkv         # confirm a real H.265 stream
```

`wf-recorder` uses `wlr-screencopy` directly (no portal/D-Bus) — the fastest way to
de-risk the GPU encode path. **Note:** screencopy encodes straight to a file and *cannot*
feed PipeWire; the real integration uses the ScreenCast portal (see M0). If shell 1 logged
a Mesa/EGL fallback (or Sway dropped to pixman) instead of `EGL vendor: NVIDIA`, install the
NVIDIA GL userspace (§2) — the portal cannot capture a pixman output.

**An idle headless output produces no frames** (its frame clock is driven by damage); give
it a real refresh mode (`prepare-session.sh` does) *and* run something animated
(`swaymsg exec foot`) or the capture will be ~1 frame.

The wlroots-on-NVIDIA env workarounds (`WLR_RENDERER=gles2`, `WLR_NO_HARDWARE_CURSORS=1`,
`GBM_BACKEND=nvidia-drm`, `sway --unsupported-gpu`, …) live in
`scripts/headless/env.sh` — `source` it before launching anything Wayland.

## 4. M0 proper — wire it into `punktfunk-core`

Goal (plan §8): headless output → PipeWire ScreenCast → NVENC → a playable file, then feed
the encoded access units into a `punktfunk_core::Session` (host role). The module seams exist
in `crates/punktfunk-host/src/{vdisplay,capture,encode,inject,pipeline}.rs`.

**Status: implemented and verified end-to-end** in `crates/punktfunk-host` (`m0.rs`,
`capture/linux.rs`, `encode/linux.rs`). After the §3 bring-up:

```sh
source /tmp/punktfunk-sway-env.sh
swaymsg exec foot   # animated content
# Live portal capture → NVENC HEVC → playable file, with each AU also round-tripped
# through a punktfunk_core host→client Session (FEC + packetize + reassemble) and verified:
cargo run -p punktfunk-host -- m0 --source portal --seconds 5 --out /tmp/punktfunk-m0.h265
ffprobe /tmp/punktfunk-m0.h265
# No capture session needed (encode + core only): --source synthetic
```

Verified result: `1920x1080` HEVC, ~300 frames in 5s, `punktfunk-core loopback … 0 mismatches`.
The portal negotiates packed **`RGB` (24-bit, 3 bpp)** on wlroots; the encoder expands it to
`rgb0` (one pad byte/pixel, no colour math) since NVENC accepts `rgb0`/`bgr0` but not
`rgb24`. dmabuf zero-copy import is still deferred (plan §9) — this is the CPU-copy path.

Crate choices, verified current:
- **Capture (portal path):** [`ashpd`](https://docs.rs/ashpd) **0.13** with the
  `screencast` feature (the `pipewire` feature is *not* needed — `open_pipe_wire_remote`
  is unconditional). Flow (0.13 API, verified against the vendored source): `Screencast::new`
  → `create_session(Default)` → `select_sources(&session, SelectSourcesOptions::default()
  .set_sources(BitFlags::from_flag(SourceType::Monitor))…)` → `start(&session, None,
  Default)` → `.response()?` → `Stream::pipe_wire_node_id()` + `open_pipe_wire_remote()`.
  Note 0.13 takes **options structs**, not the old positional args, and defaults to the
  **tokio** runtime — drive the handshake on a *multi-thread* tokio runtime (a
  current-thread one starves zbus's reader and the portal reports "Invalid session").
  Pull frames with [`pipewire`](https://docs.rs/pipewire) **0.9** — it must match the
  pipewire crate ashpd 0.13 links (the `pipewire-sys` `links` key is unique per build, so
  `0.10` fails to resolve). 0.9 uses `MainLoopRc`/`ContextRc::connect_fd_rc(OwnedFd)`/
  `StreamBox`. Only request `SourceType::Monitor` — the wlr backend's
  `AvailableSourceTypes` is `1` (Monitor only); asking for `Window`/`Virtual` invalidates
  the session. Set `XDG_CURRENT_DESKTOP=sway` so the wlr portal backend is chosen, and
  import it into the portal's environment (see "Portal bring-up" below).
- **Encode:** [`ffmpeg-next`](https://crates.io/crates/ffmpeg-next) **8.x** (binds the
  system FFmpeg 8.x via pkg-config; needs `clang`/`libclang`). Select the encoder by
  name — `encoder::find_by_name("hevc_nvenc")`, *not* by codec id (that's the SW encoder).
  Low-latency opts: `preset=p1`, `tune=ull`, `rc=cbr`, `bf=0`, `delay=0`, large `g`.
  If your FFmpeg is in a non-standard prefix, `export FFMPEG_DIR=/that/prefix`.
- **Zero-copy is the hard part.** There's no direct dmabuf→CUDA import in FFmpeg.
  **Start with the CPU-copy fallback** (download frame → `hwupload_cuda` → `hevc_nvenc`)
  to get an end-to-end stream, then chase true dmabuf zero-copy. The plan flags this
  (§9) and the `capture` module already has a `cpu_bytes` fallback field.
- **Input (M2):** [`reis`](https://crates.io/crates/reis) (pure-Rust libei — no native
  `libei` needed) with `input-linux`/uinput as the universal fallback.

Then continue toward **M2**: `serverinfo`/RTSP/pairing enough for a stock Moonlight client
to connect, a KWin virtual output created on connect, input via reis/uinput — the
shippable milestone.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Sway aborts on NVIDIA | add `--unsupported-gpu` (the helper scripts do) |
| `not a KMS device` / no connectors | expected on a headless VM GPU — use `WLR_BACKENDS=headless`, not the DRM backend |
| Sway won't start at all | `WLR_RENDERER_ALLOW_SOFTWARE=1 WLR_RENDERER=pixman` to prove the pipeline, then fix EGL |
| ScreenCast portal finds no output | ensure `xdg-desktop-portal-wlr` is running in the same session, `XDG_CURRENT_DESKTOP=sway`, and `~/.config/xdg-desktop-portal-wlr/config` has `output_name=HEADLESS-1` |
| `Cannot load libnvidia-encode.so.1` | NVENC runtime lib missing (driver) or unlicensed vGPU |
| `cargo build` can't find FFmpeg | `export FFMPEG_DIR=$(pkg-config --variable=prefix libavcodec)` or point `PKG_CONFIG_PATH` at the custom build |
| bindgen: libclang not found | `export LIBCLANG_PATH=$(llvm-config --libdir)` |
