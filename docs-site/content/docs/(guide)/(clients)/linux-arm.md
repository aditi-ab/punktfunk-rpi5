---
title: Linux on ARM
description: What the Punktfunk app decodes in hardware on ARM Linux devices — Steam Frame, Raspberry Pi 5, Rockchip boards, Snapdragon laptops, Apple silicon — and what it decodes on the processor instead.
---

Find what your ARM Linux device decodes in hardware, and what to expect where it can't.

[Install the app](/docs/install-client) as on any Linux system. It needs no ARM-specific setup: it
asks the device's video decoder what it takes and tells the host, so the host picks a codec the
device decodes in hardware.

## What decodes where

| Device | Decodes in hardware |
|---|---|
| Steam Frame | H.264 and HEVC; AV1 and 10-bit where the headset's system offers them |
| Snapdragon X laptops | H.264 and HEVC |
| Raspberry Pi 5 | HEVC, 8-bit |
| Rockchip RK3588 boards | HEVC, 8-bit |
| Apple silicon (Asahi Linux) | Nothing: H.264 on the processor |

Three limits come from the device's Linux drivers, not from Punktfunk:

- **Raspberry Pi 5** decodes HEVC on Raspberry Pi OS's kernel only, and every frame is copied once
  on its way to the screen.
- **Rockchip RK3588** needs kernel 7.0 or newer. Vendor kernels have no decoder the app can use.
- **Snapdragon X** laptops lose their decoder on installs that boot Linux as a hypervisor.

NVIDIA Jetson boards are not supported.

HDR and 10-bit need a decoder with a 10-bit output. Where the device has none, the app leaves them
out of what it asks the host for, whatever the HDR setting says. Processor decoding is 8-bit
only; see the [Support matrix](/docs/support-matrix#client-decode).

## Check which decoder runs

The [stats overlay](/docs/stats) names the decoder at its **Detailed** level. `native-v4l2` is the
device's hardware decoder; `software` is the processor.

To pin the hardware decoder or name its device node, set
[`PUNKTFUNK_DECODER` or `PUNKTFUNK_V4L2_DEVICE`](/docs/configuration#client-side-native-clients).

## Troubleshooting

### The overlay says `software` on a device with a hardware decoder

The app found no decoder node it can open. Check that `ls /dev/video* /dev/media*` lists devices
and that your user may open them; on most systems that means being in the `video` group.

### The Steam Frame's controllers do nothing

Outside Steam the headset's controllers are not a gamepad. Add Punktfunk to Steam as a non-Steam
game and start it from your library; Steam then passes them on as one.
