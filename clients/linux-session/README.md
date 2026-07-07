# punktfunk-session

The Vulkan session binary: one stream per invocation in an SDL3 window — no UI toolkit,
no widgets, terminal stats. The power-user / gamescope stream client, and the stage-2
presenter of the Linux client re-architecture (punktfunk-planning:
`linux-client-rearchitecture.md`).

```
punktfunk-session --connect host[:port] [--fp HEX] [--launch id] [--fullscreen] [--stats]
punktfunk-session --browse host[:port] [--mgmt PORT] [--fullscreen]
```

`--browse` opens the console game library (the Skia coverflow over the animated aurora)
instead of connecting: A launches the focused title as a stream in the same window,
session end returns to the library, B quits (Gaming Mode returns). Paired hosts only —
pairing is the desktop client / Decky plugin's job. `PUNKTFUNK_FAKE_LIBRARY=<file.json>`
feeds canned entries with no host (portrait paths starting with `/` load from disk).

Reads the same identity / known-hosts / settings stores as the desktop client
(`punktfunk-client`) — pair there (or via its headless `--pair`) first; this binary never
connects to a host it has no pinned fingerprint for (`--fp HEX` overrides the store).

Stdout is the machine interface: `{"ready":true}` after the first presented frame,
`stats: …` once per second (Ctrl+Alt+Shift+S toggles, `--stats` forces on), one
`{"error"|"ended": …}` JSON line on the way out. Logs go to stderr. Exit codes: `0`
clean end, `2` connect failed, `3` trust rejected / pairing required, `4` presenter
init failed.

In-stream keys match the desktop client: click captures input (Ctrl+Alt+Shift+Q
releases), Ctrl+Alt+Shift+D disconnects, F11 toggles fullscreen; the controller escape
chord (L1+R1+Start+Select, hold to disconnect) works the same.

The default build carries the Skia console UI (`ui` feature): the stats OSD and capture
hint render in-window (Ctrl+Alt+Shift+S toggles both the OSD and the stdout mirror).
`--no-default-features` is the ~5 MB power-user build — same streaming, stats on stdout
only, no Skia anywhere in the dependency tree.

Decode follows the Settings preference (auto: Vulkan Video → VAAPI → software):
FFmpeg's Vulkan Video decoder runs on the presenter's own device where the stack
supports it (every vendor, zero copy); VAAPI dmabufs import per-plane elsewhere;
software is the universal fallback. 10-bit Main10 and HDR10 are advertised
(`VIDEO_CAP_10BIT|HDR`): P010 decodes through all three paths, and PQ streams present
on an HDR10/ST.2084 swapchain when the desktop offers one (KDE HDR, gamescope) or
tone-map in-shader to SDR when it doesn't (`PUNKTFUNK_TONEMAP_PEAK` tunes the rolloff,
default ≈1000 nits). The host still gates the upgrade behind its `PUNKTFUNK_10BIT`
policy.

Debug/bisect knobs: `PUNKTFUNK_DECODER=vulkan|vaapi|software`, `PUNKTFUNK_PRESENT_MODE=
mailbox|immediate` (default FIFO), `PUNKTFUNK_VK_DEVICE=<index>` (multi-GPU), and
`PUNKTFUNK_HW_FAULT=import` (fault every VAAPI dmabuf import — proves the three-strike
demotion to software on healthy hardware).
