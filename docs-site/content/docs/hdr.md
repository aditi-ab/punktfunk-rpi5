---
title: HDR
description: How an HDR10 stream is decided end to end — the four things that must all be true, what each host and client can actually do, and how to check which link is missing.
---

An HDR session carries a **10-bit BT.2020 PQ (HDR10)** picture from the host's display to your
screen. It is on by default wherever it works, and a session that can't be HDR streams 8-bit BT.709
SDR instead. Which one you get is decided **before the first frame**: the host resolves every gate
below, then tells the client what it is really going to send. Nothing on this page takes effect
mid-stream, so reconnect after changing any of it.

## The chain

Four things must all be true. If your stream is SDR when you expected HDR, one of these is why.

1. **The source.** What the host captures must hand it 10-bit PQ pixels. This is the link that fails
   most often, and it is entirely a host-side question — see [Per host](#per-host).
2. **The encoder.** The host GPU must encode 10-bit for the codec the session picked. The host
   probes this by opening a tiny real encoder once per GPU and codec, and believes the answer.
   (PyroWave skips the probe — it has its own rule, below.)
3. **The codec.** Only HEVC, AV1 and PyroWave have a 10-bit path — see [Codec rules](#codec-rules).
4. **The client.** Your client must advertise 10-bit and HDR (its **HDR** setting), and be able to
   present or tone-map PQ.

The host allows 10-bit by default (`PUNKTFUNK_10BIT`). It only ever *allows* — the client's setting
is the real per-session switch.

## Per host

### Windows

The Windows host creates a virtual display for your session and **turns HDR on for that display
itself** when the session negotiated 10-bit. You do not have to enable "Use HDR" in Windows Settings
first: Punktfunk enables advanced colour at capture open, waits for it to settle, then composes in
FP16 and encodes P010 BT.2020 PQ. The reverse is enforced too — a session that negotiated **SDR**
forces advanced colour **off** on that display, so a client that asked for 8-bit is never handed PQ.
If enabling it fails, the host logs a loud error and encodes 8-bit anyway: the client was already
told HDR, so that is the one place a Punktfunk label can outrun the picture. The log says so.

Two details worth knowing:

- **HDR and 4:4:4 compose on Windows, not on Linux.** A **Windows** host carries both: the capture
  path writes full-resolution 10-bit chroma and NVENC encodes HEVC Main 4:4:4 10, so
  [full chroma](/docs/client-settings) costs you nothing on an HDR desktop.
  [PyroWave](/docs/pyrowave) does the same there, in 16-bit planes. On **Linux** the 4:4:4 route is
  8-bit, so a session that negotiates both resolves back down to SDR — full chroma wins. AV1 never
  carries 4:4:4 anywhere: Range Extensions are HEVC-only.
- **Vulkan games need the bundled layer.** NVIDIA and AMD Vulkan drivers refuse to advertise any HDR
  colour space for a surface on an indirect (virtual) display, so Vulkan games decide the device
  "does not support HDR" — even though the driver happily presents an HDR swapchain there. The host
  installer ships an implicit Vulkan layer, `VK_LAYER_PUNKTFUNK_hdr_inject`, that adds those formats
  back (installer task **Install the HDR Vulkan layer**, ticked by default). It self-gates on the
  monitor's live advanced-colour state, so it does nothing on an SDR session, and it already skips a
  built-in list of kernel-anti-cheat titles. `DISABLE_PF_VKHDR=1` in a game's environment switches it
  off for that process; `PF_VKHDR_EXCLUDE=foo.exe,bar.exe` skips further executables by name.
  D3D11/D3D12 games need none of this.

### Linux + gamescope

A stock gamescope tone-maps its composite down to 8 bits before handing it over, so its capture
output is SDR no matter what the game rendered. Real HDR needs **`punktfunk-gamescope`**, a build
carrying a patch that adds the 10-bit PQ formats to its PipeWire node. It installs beside your system
gamescope rather than replacing it; [HDR on gamescope](/docs/gamescope#hdr-on-gamescope) has the
package for each distro.

The host settles two facts before spawning anything: the gamescope binary it will run carries the
patch (its `--version` banner contains `+pfhdr`), and this host is the one **starting** the session
rather than attaching to a node someone else started.

**Attach mode is the trap.** The patched build only reaches sessions the host spawns itself —
managed, `PUNKTFUNK_GAMESCOPE_SESSION`, or a bare spawn. A session started by your display manager
runs the distro's own gamescope, which offers neither the 10-bit formats nor the in-node cursor. The
host cannot tell that from the outside unless you pinned `PUNKTFUNK_GAMESCOPE_NODE`: with
`PUNKTFUNK_GAMESCOPE_ATTACH=1` and the patched build installed it reads the binary, believes HDR is
available, and offers it. The attached session can't answer that negotiation, so the connect fails
with no picture, the host latches an SDR downgrade for the rest of its life, and the next connect
streams — in SDR.

That combination bites on [Bazzite](/docs/bazzite), where the sysext installs `punktfunk-gamescope`
alongside a stock session gamescope. No template pins attach any more, so the managed default gets
you HDR and the compositor-drawn cursor — but an older template did, and an upgrade never rewrites a
`host.env` you already have, so check yours for `PUNKTFUNK_GAMESCOPE_ATTACH=1` and delete the line.
If you deliberately stay on attach, set `PUNKTFUNK_GAMESCOPE_HDR=0` so the failed attempt never
happens. Staying on attach also leaves the stream with no cursor;
[HDR on gamescope](/docs/gamescope#hdr-on-gamescope) has the fix for that half.

SDR content rides the same PQ container — the desktop, the Steam overlay, an SDR game — mapped in at
`PUNKTFUNK_GAMESCOPE_SDR_NITS` (gamescope's own default is 400). That is the knob when white looks
too bright or too dim on your TV.

### Linux + GNOME

A Punktfunk host serves [two protocols](/docs/how-it-works#two-protocols): its own `punktfunk/1`,
which the Linux, Windows, Apple and Android apps speak, and GameStream, which
[Moonlight](/docs/moonlight) speaks. GNOME HDR is available on the GameStream side only.

GNOME 50 added HDR screencast for **real monitors** only, so this route mirrors a monitor instead of
creating a virtual display: set `PUNKTFUNK_VIDEO_SOURCE=portal`, put the monitor in HDR mode in
**Settings → Displays**, and connect an HDR-capable client. `PUNKTFUNK_CAPTURE_MONITOR=<connector>`
pins which head, and when it is set the host checks *that* monitor's colour mode rather than asking
whether any monitor is in HDR. If none is, the session degrades to 8-bit SDR and says so in the log.

A Punktfunk app connecting to a GNOME host over `punktfunk/1` gets SDR. On that protocol the only
Linux HDR source is the gamescope virtual output.

### Linux virtual displays on KWin, Mutter and wlroots

**These are SDR.** Mutter's `RecordVirtual` streams and the KWin and wlroots virtual outputs are
8-bit upstream, so there is nothing for the host to capture in 10 bits — no setting changes this.
Streaming a *physical* monitor with the [Streamed screen](/docs/virtual-displays) setting is SDR to
a Punktfunk app too, HDR panel or not; the GNOME/GameStream route above is the only Linux monitor
mirror that can be HDR.

## Per client

| Client | HDR10 present | Advertises HDR when |
|---|---|---|
| **Linux** (GTK) · **Windows** (WinUI 3) | Yes — the Vulkan presenter switches to an HDR10 swapchain when the surface offers one | the setting is on. It does **not** check your display first |
| **macOS · iPhone · iPad** | Yes — Metal, `rgba16Float` in BT.2100 PQ with EDR | the setting is on **and** the display reports HDR capability |
| **Apple TV** | Yes — PQ passthrough once the session's display-mode switch lands; before that, tone-mapped to SDR in-shader | the setting is on **and** the TV is HDR-capable |
| **Android** (phone + TV) | Yes — HDR10 via the Surface dataspace plus static metadata | the setting is on **and** the panel reports HDR10 or HDR10+. On an SDR panel the toggle is disabled |
| **Moonlight** | Its own HDR toggle, which appears only when the host advertises a 10-bit codec | — |

The Linux and Windows clients are deliberately looser: they advertise HDR whenever the setting is on
and let the presenter sort out the display side — HDR10 swapchain where the compositor offers one,
tone-mapped to SDR where it doesn't. The stats overlay says which happened: `HDR` versus `HDR→SDR`.

One exception: frames from **software decode** never take the HDR10 swapchain, whatever the surface
offers. On a client with no hardware HEVC decode an HDR stream is therefore presented on the SDR
swapchain without a tone-map, which looks washed out. Turn the client's HDR setting off there. The
[Steam Deck plugin](/docs/steam-deck) streams through this same client.

## Codec rules

- **HEVC** — Main10. The usual HDR codec.
- **AV1** — 10-bit, where the GPU encodes it. Advertised separately from HEVC, so a box that does
  one and not the other tells the truth about each.
- **H.264** — never. High10 is not an encode mode on the hardware Punktfunk targets, so negotiation
  never even asks. Pinning H.264 in your client settings pins the session to SDR.
- **[PyroWave](/docs/pyrowave)** — carries HDR in 16-bit planes, but **only from a Windows host**.
  The Linux PyroWave capture path has no HDR colour conversion, so a Linux-hosted PyroWave session
  is SDR. Use HEVC or AV1 for HDR from Linux.

One more rule if you also use full chroma: a **Linux** host encodes 4:4:4 at 8 bits, so a session
that negotiates both resolves back down to SDR before the stream starts — on Linux, 4:4:4 wins. A
**Windows** host has no such trade: it carries HDR and full chroma at once. Full chroma is off until
you turn it on, so this only bites if you did.

## Check it

On a **Linux** host, one subcommand answers every link in the chain:

```bash
punktfunk-host hdr-probe
```

It prints, line by line: whether a monitor is in BT.2100 colour mode, whether the resolved gamescope
offers 10-bit PQ capture, the state of `PUNKTFUNK_GAMESCOPE_HDR`, whether gamescope will paint the
cursor into the capture node, the encoder's Main10 answer for HEVC and for AV1, whether the resolved
compositor can do HDR on Punktfunk's own plane, and the combined GameStream capability.

**Run it with the environment the host service has.** The service loads `host.env` itself; your shell
does not, and `PATH` decides which gamescope binary gets probed:

```bash
set -a; . ~/.config/punktfunk/host.env; set +a
punktfunk-host hdr-probe
```

There is no `hdr-probe` on Windows. What Windows has instead is a GPU colour self-test for the
capture conversion, which needs no display or session:

```powershell
punktfunk-host hdr-p010-selftest 1920x1080 nvidia
```

Pass your real capture size (heights like 1080 are not 16-aligned and take a different driver path)
and, on a dual-GPU box, the vendor that encodes — `intel`, `nvidia` or `amd`.

From the client side, set the [stats overlay](/docs/stats) to **Detailed**. On Linux and Windows it
adds an `HDR` tag — or `HDR→SDR` when a PQ stream is arriving but the local surface can't present
it. Android prints the negotiated depth and colour outright at the same tier
(`HEVC · 10-bit · HDR (BT.2020 PQ) · 4:2:0`).

## The switches

Host, in [`host.env`](/docs/configuration):

| Setting | Default | Effect |
|---|---|---|
| `PUNKTFUNK_10BIT` | **on** | Allow 10-bit (HEVC Main10 / AV1 10-bit) at all. `0`, `false`, `off` or `no` forces every session to 8-bit SDR. |
| `PUNKTFUNK_GAMESCOPE_HDR` | **on** | Allow HDR on the gamescope backend. It only decides whether HDR is *attempted* — a host without `punktfunk-gamescope` stays SDR either way. `0` is the escape hatch that puts the gamescope backend back on the old SDR path, spawn flags included. |
| `PUNKTFUNK_GAMESCOPE_SDR_NITS` | gamescope's own (400) | How bright SDR content is inside the PQ container of an HDR gamescope session. |
| `PUNKTFUNK_VIDEO_SOURCE=portal` | unset | Required for the GNOME 50+ monitor-mirror route. GameStream/Moonlight only — it has no effect on `punktfunk/1` sessions. |

Client: one toggle, in Settings under **Quality** with
[the rest of the video settings](/docs/client-settings#video) — **10-bit HDR** on the Linux, macOS,
iOS, iPadOS and tvOS apps, **HDR (10-bit, BT.2020 PQ)** on Windows, **HDR** on Android. It is **on
by default** on all of them. Turning it off means "never send me 10-bit", and the host then never
upgrades the session. Like the other video settings it can be set per
[profile](/docs/profiles-and-links), so a Work profile can prefer 4:4:4 while a Couch profile
prefers HDR.
