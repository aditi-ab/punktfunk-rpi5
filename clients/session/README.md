# punktfunk-session

The Vulkan session binary: one stream per invocation in an SDL3 window, no UI toolkit. It is
deliberately dumb — a renderer the front-ends call *into*. The GTK shell, the WinUI shell and the
`punktfunk` CLI all spawn it through the same brain (`pf_client_core::orchestrate`), which resolves
policy (presets, settings, wake) and hands the result down, normally as a `--resolved-spec` file.
It reads the shared stores only as the compat fallback for a bare hand-launched invocation.

`src/main.rs`'s module docs own the shell↔session contract — the stdout JSON lines, the `stats:`
mirror and the exit codes. Read them there rather than trusting a copy here.

```
punktfunk-session --connect host[:port] [--fp HEX] [--launch id] [--fullscreen] [--stats]
punktfunk-session --browse host[:port] [--mgmt PORT] [--fullscreen]
```

`--browse` opens the console game library instead of connecting: A launches the focused title in the
same window, session end returns to the library, B quits. Paired hosts only — pairing is the
desktop client's or the Decky plugin's job, and `punktfunk pair <host>` is the supported ceremony
(`--pair` here still works for one release, with a deprecation notice, because someone's
provisioning script calls it).

This binary never connects to a host it has no pinned fingerprint for; `--fp HEX` overrides the
store.

The default build carries the Skia console UI (`ui` feature) for the stats OSD and capture hint;
`--no-default-features` is the ~5 MB power-user build with stats on stdout only and no Skia in the
dependency tree.

## Decode rungs

`auto` walks native rungs in device-specific order: NVIDIA/AMD try pf-vkdecode over Vulkan
Video first; Intel/unknown try the platform rung first (pf-dxvadec on Windows, pf-vaapi on Linux).
Admission may skip a rung before the CPU fallback (openh264/rav1d). **There is no FFmpeg in this
binary.**

The confidence used to prioritize rung/codec pairs lives in
`pf_client_core::video::native_evidence`. Its note names the hardware evidence and remaining gap;
the same facts appear in the session log:

```
decode rung active  rung=native-vulkan codec=HEVC automatic_priority_verified=true evidence=...
```

That line is a **WARNING** when the active pair is below the automatic-priority evidence bar. Read
any field report against the table, not against prose here.

## Dev knobs

`PUNKTFUNK_DECODER` and `PUNKTFUNK_PRESENT_MODE` are user-facing and documented on the
[docs site](https://docs.punktfunk.unom.io/docs/configuration). These are not:

| Variable | Effect |
| --- | --- |
| `PUNKTFUNK_VK_DEVICE=<index>` | Pick the GPU on a multi-GPU box. |
| `PUNKTFUNK_TONEMAP_PEAK` | Rolloff for tone-mapping PQ to an SDR swapchain (default ≈1000 nits). |
| `PUNKTFUNK_HW_FAULT=import` | Fault every VAAPI dmabuf import — proves the three-strike demotion to software on healthy hardware. |
| `PUNKTFUNK_AU_FAULT=drop\|truncate\|flip[:period]` | Corrupt decoder input on the native Vulkan lane (default period 60). |
| `PUNKTFUNK_FAKE_LIBRARY=<file.json>` | Feed `--browse` canned entries with no host. |

The three `AU_FAULT` modes fail in different places on purpose: `drop` swallows an AU so the next
one references a picture that was never decoded — the bitstream planner catches it immediately;
`truncate` and `flip` both parse perfectly, so only the driver's per-frame decode-status query sees
them, and neither is visible at all on a driver without `queryResultStatusSupport`. Watch the
Detailed stats line's `integrity:` term. Note that `PUNKTFUNK_AU_DUMP` records the AU as it arrived
from the host, *before* the injector runs, so a faulted run's dump is the clean bitstream.

`fixtures/mixed-platform-library.json` is the standing asset for library grouping and sorting:
launchers, five platforms, several stores, and entries with no platform at all — the case that
matters, because a platform-less Steam library must not collapse into one "Unknown" heap. Two
titles are deliberately awkward ("The Witcher 3" sorts under W, "Émigré" under E) so a broken
article fold or diacritic relaxation shows on screen rather than only in a unit test.
