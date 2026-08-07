# punktfunk-session

The Vulkan session binary: one stream per invocation in an SDL3 window — no UI toolkit,
no widgets, terminal stats. The power-user / gamescope stream client, and the stage-2
presenter of the Linux client re-architecture (punktfunk-planning:
`linux-client-rearchitecture.md`).

This binary is deliberately dumb: a renderer the front-ends call INTO — the GTK shell
(`punktfunk-client`), the WinUI shell, and the `punktfunk` CLI all spawn it through the
same brain (`pf_client_core::orchestrate`), which resolves policy (profiles, settings,
wake) and hands the result down, normally as a `--resolved-spec` file. It reads the
shared stores only as the compat fallback for a bare hand-launched invocation.

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
(`punktfunk-client`), so enrolling on either side makes the other work; this binary never
connects to a host it has no pinned fingerprint for (`--fp HEX` overrides the store).

Pairing is `punktfunk pair <host>` — the CLI, which ships alongside this binary in every
package and needs no window and no toolkit either. `punktfunk-session --pair` still works
for one release (someone's provisioning script calls it today) but prints a deprecation
notice: pairing is a trust ceremony and belongs to the brain, not a renderer.

Stdout is the machine interface: `{"ready":true}` after the first presented frame,
`stats: …` once per second while the overlay tier isn't Off (always the full detailed
text, whatever the OSD shows; `--stats` forces the overlay on), one
`{"error"|"ended": …}` JSON line on the way out. Logs go to stderr. Exit codes: `0`
clean end, `2` connect failed, `3` trust rejected / pairing required, `4` presenter
init failed.

In-stream keys match the desktop client: click captures input (Ctrl+Alt+Shift+Q
releases), Ctrl+Alt+Shift+D disconnects, F11 toggles fullscreen; the controller escape
chord (L1+R1+Start+Select, hold to disconnect) works the same.

The default build carries the Skia console UI (`ui` feature): the stats OSD and capture
hint render in-window. Ctrl+Alt+Shift+S cycles the OSD tier live — Off → Compact (one
line: fps · latency · Mb/s) → Normal (mode + end-to-end percentiles) → Detailed (decoder
path + per-stage latency equation); any tier but Off also emits the stdout mirror.
`--no-default-features` is the ~5 MB power-user build — same streaming, stats on stdout
only, no Skia anywhere in the dependency tree.

Decode follows the Settings preference (auto is vendor-ordered: Vulkan Video → VAAPI →
software on Linux, Vulkan Video → D3D11VA → software on Windows, with VAAPI/D3D11VA first
on Intel — every rung native since M10; see "Decode rungs" below): the Vulkan decoder runs
on the presenter's own device where the stack supports it (every vendor, zero copy); VAAPI
dmabufs import per-plane elsewhere (D3D11VA textures on Windows); software is the universal
fallback. 10-bit Main10 and HDR10 are advertised (`VIDEO_CAP_10BIT|HDR`): P010 decodes
through the Vulkan and VAAPI/D3D11VA paths (the CPU rung is 8-bit by contract and refuses
10-bit rather than mis-scaling it), and PQ streams present
on an HDR10/ST.2084 swapchain when the desktop offers one (KDE HDR, gamescope) or
tone-map in-shader to SDR when it doesn't (`PUNKTFUNK_TONEMAP_PEAK` tunes the rolloff,
default ≈1000 nits). The host still gates the upgrade behind its `PUNKTFUNK_10BIT`
policy.

## Decode rungs (M10: native only)

**This binary contains no FFmpeg.** `auto` walks native rungs — pf-vkdecode over Vulkan
Video, then the platform's own (pf-dxvadec on Windows, pf-vaadec on Linux), then the CPU
rung (openh264/rav1d). The libavcodec rungs that used to sit under each of them are
deleted, along with `pf-ffvk` and the `ffmpeg-next` dependency.

One of the native rungs has never decoded a frame on real hardware (native VAAPI's H.264 and
H.265 legs; its AV1 leg has decoded but has never been parity-checked). It runs anyway — with the libavcodec twins gone, the only
thing below them is the CPU, so barring them would cost the session hardware decode
outright rather than move it one rung down. What replaces the safety net is the log: every
session names the rung it landed on with its evidence state,

    decode rung active  rung=native-vulkan codec=HEVC hardware_verified=true evidence=...

…and that line is a **WARNING** when nothing has ever decoded a frame through the
rung/codec pair the session chose. `pf-client-core`'s `video.rs` module docs carry the full
table; read any field report about M10 against it.

Debug/bisect knobs: `PUNKTFUNK_DECODER=native-vulkan|native-vaapi|native-d3d11va|software`
(a pin skips the vendor order, which is how a lab run reaches a rung `auto` will not pick
on this device; a pinned rung that cannot open still falls through to the standard ladder,
loudly; `native-vaapi` also takes `PUNKTFUNK_VAAPI_DEVICE=/dev/dri/renderDNNN` to choose
the GPU). The pre-M10 spellings `vulkan`/`vaapi`/`d3d11va` named the libavcodec rungs
specifically; they are MIGRATED onto the native rung for the same hardware family, with a
`warn` line saying so — every desktop Settings UI offered those values, so refusing them
would end a session over a dropdown someone picked long ago.
`PUNKTFUNK_PRESENT_MODE=
mailbox|fifo|immediate|fifo_relaxed` (default MAILBOX, FIFO where the surface offers no
MAILBOX — AMD on Windows), `PUNKTFUNK_VK_DEVICE=<index>` (multi-GPU), and
`PUNKTFUNK_HW_FAULT=import` (fault every VAAPI dmabuf import — proves the three-strike
demotion to software on healthy hardware).

`PUNKTFUNK_AU_FAULT=drop|truncate|flip[:period]` deliberately corrupts decoder input on the
native Vulkan lane (default period 60 — one AU a second at 60 fps; inert everywhere else, and
inert entirely if the value doesn't parse). `drop` swallows the AU, so the next one references a
picture that was never decoded — the bitstream planner catches it immediately. `truncate` delivers
a picture whose slice data stops mid-frame and `flip` alters one byte deep in the payload: both
parse perfectly, so only the driver's per-frame decode-status query can see them, and neither is
visible at all on a driver without `queryResultStatusSupport`. Watch the
result on the Detailed stats line's `integrity:` term (`damaged` = concealment the planner caught,
`refused` = AUs the decoder rejected outright, `driver-failed` = the hardware's own verdict, `run`
= consecutive frames with no picture, `worst run` = the longest such stretch of the session — the
once-a-second `run` sample misses the bad moment almost every time — and `no driver status` = this
device cannot answer the driver question at all). A session that lands on any other lane says so
in the log rather than faulting silently.

Note that `PUNKTFUNK_AU_DUMP` records the AU as it arrived from the HOST, while the fault injector
runs later, at the native decoder's own entry. On a faulted run the dump is therefore the clean
bitstream — reconstruct the damaged bytes from the spec if you need them (the injector is pure and
deterministic).
