# `punktfunk-gamescope` — gamescope with 10-bit HDR PipeWire capture

Upstream gamescope's built-in PipeWire node is SDR-only: `build_format_params()` offers `BGRx`
and `NV12`, and `paint_pipewire()` hardcodes a Gamma-2.2 composite with the SDR screenshot LUT
set. An HDR game therefore reaches every capture consumer already tone-mapped down — which is
why the punktfunk gamescope backend has always streamed 8-bit, even though games *can* render
HDR on a headless gamescope today (`--hdr-enabled --hdr-debug-force-support`).

The patches here add the missing half, and nothing else. See
`punktfunk-planning/design/gamescope-hdr-virtual-output.md` for the full design.

| Patch | What | Upstream? |
|---|---|---|
| `0001-pipewire-offer-10-bit-BT.2020-PQ-capture-formats-HDR.patch` | Offer SPA `xRGB_210LE`/`xBGR_210LE` with MANDATORY SMPTE ST.2084 + BT.2020 props, map them to `DRM_FORMAT_XRGB2101010`/`XBGR2101010`, and composite them with `g_ScreenshotColorMgmtLutsHDR` + `EOTF_PQ` | **Yes** — offered against [gamescope#2126](https://github.com/ValveSoftware/gamescope/issues/2126) |
| `0002-punktfunk-stamp-the-version-banner-with-pfhdr1.patch` | Append `+pfhdr1` to the `--version` banner | **No** — ours only, retired when patch 1 lands upstream |

## Why the marker exists

punktfunk decides a session's bit depth at handshake time, **before** the virtual display
exists, and the Welcome is irrevocable (a PQ stream handed to an 8-bit encoder is a deliberate
hard error). The capability answer must therefore be a static property of the resolved binary,
not an optimistic negotiation. The host runs `<gamescope> --version` once per boot and looks for
`+pfhdr` in the banner — see `gamescope_hdr_capable()` in
`crates/pf-vdisplay/src/vdisplay/linux/gamescope/discovery.rs`.

Bump the number (`+pfhdr2`, …) only if the patch's **wire** behaviour changes; the host's probe
accepts any `+pfhdr<N>`.

## Which binary the host runs

Resolution order, applied identically by the bare spawn, the `GAMESCOPE_BIN` wrapper
(gamescope-session-plus) and the SteamOS PATH shim:

1. `PUNKTFUNK_GAMESCOPE_BIN` — absolute path override
2. `punktfunk-gamescope` on `PATH`
3. `gamescope`

So installing this build under the name `punktfunk-gamescope` is enough; nothing replaces the
distro's `gamescope`.

## Building

Pinned upstream: `8c676c39` (master, 2026-07-27 — tags through 3.16.25). The patches apply
cleanly to that commit; they touch `src/pipewire.cpp`, `src/steamcompmgr.cpp` and
`src/meson.build` only.

```sh
git clone https://github.com/ValveSoftware/gamescope.git
cd gamescope
git checkout 8c676c39
git submodule update --init --recursive          # or let meson fetch the subprojects
git am /path/to/punktfunk/packaging/gamescope/patches/*.patch

meson setup build/ --prefix=/usr -Dpipewire=enabled
ninja -C build/
# install as punktfunk-gamescope, NOT as gamescope
install -Dm755 build/src/gamescope /usr/bin/punktfunk-gamescope
```

`gamescope` needs `CAP_SYS_NICE` for its realtime priority; the distro packages set it on their
own binary. Mirror it if you install ours system-wide:

```sh
setcap 'CAP_SYS_NICE=eip' /usr/bin/punktfunk-gamescope
```

## Verifying the patch on a box (P0 exit)

```sh
punktfunk-gamescope --version                    # must contain +pfhdr1
punktfunk-gamescope --backend headless -W 1920 -H 1080 -r 60 \
    --hdr-enabled --hdr-debug-force-support -- vkcube &
pw-dump | grep -A40 '"gamescope"'                # node offers xRGB_210LE / xBGR_210LE
```

The stream is only 10-bit once a **consumer** asks for it: the formats are listed last, so any
consumer that negotiates the 8-bit stream today keeps negotiating it bit-for-bit.

## Rebase policy

The functional patch is two files and mirrors code that already exists in-tree (the HDR AVIF
screenshot path), so it rebases cheaply. We pin the gamescope commit we ship; when upstream
takes it, both patches are dropped and the host's capability probe becomes a plain version
floor.
