# Raspberry Pi 5 menu cadence

Output: 3840 × 1600 at 60 Hz, native-resolution text and controls, hardware
V3D rendering and Weston. Measurements use real keyboard events and DRM delivery
timestamps on the Pi. No manual navigation was required.

## Changes

- Rasterize tab content, pinned controls and hints once per transition; reuse
  those images for the slide and fade. New mounts and geometry changes discard
  them. Accepted focus, press and text input resumes live rendering.
- Bound pinned transparency layers to their recorded content, including shadows.
- Give Weston a maximum 16 ms repaint window instead of 12 ms.
- Render only the soft animated background at a 96-pixel long edge on Broadcom;
  the menu viewport, controls and text remain native resolution.

The production source is `e79cbf008ada189594e0412db69434f1b80b830c`, tagged
`v0.46.0-rpi5.25`. Its immutable patch snapshot is selected by `CURRENT`.

## Preflight comparison

The baseline was `v0.46.0-rpi5.14`. The candidate measurements below used
`v0.46.0-rpi5.22` with the final 96-pixel background and 16 ms compositor settings.
Each pair has 34 alternating tab inputs over 20 seconds. Transition FPS counts
deliveries during the first 300 ms of each input.

| Tab pair | Baseline transition FPS | Candidate transition FPS |
| --- | ---: | ---: |
| Hosts / Games | 27.06 | 59.22 |
| Games / Controllers | 32.06 | 59.71 |
| Controllers / Settings | 23.53 | 58.63 |

A 60-second run of 100 Controllers / Settings switches delivered 59.23 FPS
overall and 58.73 FPS during transition windows. All transition windows contained
17 or 18 deliveries. The 95th-percentile interval was 16.67 ms; the longest was
33.35 ms. Occasional missed refreshes remain; this is not a claim of zero drops.

Scrolling Settings with automated up/down input delivered 58.40 FPS over 20 seconds.
Screenshots confirmed the four distinct root pages and intact text, controls and hints.

The existing 2560 × 1440/60 HEVC 8-bit stream, targeting 150 Mbps, delivered
approximately 60 received, decoded and presented frames per second with Normal
and Detailed statistics. Both measured 20-window samples had zero lost or skipped
frames. The hidden statistics tier suppresses JSON output, so it is not counted
as another machine-readable sample.

## Final package validation

The exact release bundle is installed on the Pi. Temporary environment overrides
and Weston's debug mode were removed. The installer migrated the existing managed
12 ms setting to 16 ms, and the application logged its default backdrop edge of 96.

| Tab pair | Transition FPS | Whole capture FPS |
| --- | ---: | ---: |
| Hosts / Games | 59.12 | 59.52 |
| Games / Controllers | 59.41 | 59.72 |
| Controllers / Settings | 58.24 | 59.08 |

The final 100-switch soak passed the 55 FPS regression threshold: 59.07 FPS overall,
58.40 during transitions, 17–18 deliveries per transition window, 16.67 ms p95,
33.35 ms maximum. An automated Down input during a tab transition was captured
with its focus plate moving, then correctly settled on the next setting.

The final steady-state stream sample delivered 60.06 FPS with zero lost or skipped
frames across 20 windows. A separate startup sample retained 37 skipped frames
in its first three windows; the initial overlay still has a brief cost. Subsequent
windows were clear. The steady-state sample waits 40 seconds after restarting
the application, then reads the last 20 seconds rather than hiding startup data.

Both services remained active with zero automatic restarts. Temperature was 52.7°C,
throttling flags were zero, and the kernel log showed no GPU reset, hang or timeout.

Checks:

- 1,108 native ARM64 Rust tests passed; 28 ignored cases were not run.
- 10 frame-capture/parser tests and 5 patch-maintenance tests passed.
- Exact patch replay reproduced tree `52f383d499d067fbb7249aa8d6b13593ff149e90`.
- The full image build and offline image verification passed in
  [release run 41748189157](https://github.com/aditi-ab/punktfunk-rpi5/actions/runs/41748189157).
- The downloaded image and its metadata match their published SHA256 checksums;
  `xz -t` also passed.

Artifacts:

- `punktfunk-0.46.0-rpi5.25-linux-arm64.tar.gz` SHA256:
  `ef9f3790c7dedd9606eca000022b6a24b77de1a461071007f49b9e4fec64d74c`.
- `punktfunk-0.46.0-rpi5.25-kiosk.img.xz` SHA256:
  `1fadebbfaf82997269d00e3abfc7659e180c1004d37afeb45ec48f4d1eb9c4e1`.

Numeric samples are retained beside this report in `menu-cadence-2026-11-04/`.
Screenshots and raw host logs are kept outside the public repository.

## Limits

DRM deliveries measure display cadence; they do not by themselves establish that
every frame contains different pixels. Input tests, composition pixel tests and
screenshots provide complementary checks. The tested display mode is 3840 × 1600
at 60 Hz. Full 4K, 120 Hz and HDR are not verified by these measurements. A fresh
flash and boot of the generated image has not been performed.
