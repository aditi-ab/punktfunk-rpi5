# punktfunk-session

The Vulkan session binary: one stream per invocation in an SDL3 window — no UI toolkit,
no widgets, terminal stats. The power-user / gamescope stream client, and the stage-2
presenter of the Linux client re-architecture (punktfunk-planning:
`linux-client-rearchitecture.md`).

```
punktfunk-session --connect host[:port] [--fp HEX] [--launch id] [--fullscreen] [--stats]
```

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

Decode follows the Settings preference: VAAPI frames import zero-copy into Vulkan
(per-plane dmabuf + the stream's CICP-driven CSC shader); boxes whose driver can't
import (NVIDIA proprietary by design) fall back to software decode automatically.
Debug/bisect knobs: `PUNKTFUNK_DECODER=software|vaapi`, `PUNKTFUNK_PRESENT_MODE=
mailbox|immediate` (default FIFO), `PUNKTFUNK_VK_DEVICE=<index>` (multi-GPU), and
`PUNKTFUNK_HW_FAULT=import` (fault every dmabuf import — proves the three-strike
demotion to software on healthy hardware). HDR/P010 and the Skia console UI
(`--browse`) are later phases of the plan.
