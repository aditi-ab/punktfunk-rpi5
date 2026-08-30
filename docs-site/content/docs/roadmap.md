---
title: "Roadmap"
description: "Where Punktfunk is heading — the themes being worked on now, what comes after them, and what is deliberately not planned."
---

This page is about **intent**, not inventory. It says what the project is pushing on and why, in
themes rather than checkboxes, because a checklist of forty features is stale the week it is written.

Three other pages answer the questions people usually bring to a roadmap, and they answer them
better:

- **"Does it do X on my machine?"** — the [Support matrix](/docs/support-matrix). Every cell there
  was read out of the code that makes the decision. It is the arbiter: where this page and the
  matrix disagree, the matrix is right.
- **"What changed recently?"** — the [release notes](https://git.unom.io/unom/punktfunk/releases),
  per version.
- **"How finished is this?"** — [How finished each part is](/docs/support-matrix#how-finished-each-part-is).

**Nothing on this page is shipped unless it says so.** Recently shipped work — mid-stream microphone
mute with echo cancellation, desktop apps that decode on your GPU with no FFmpeg in them, lossless
audio, guest access with a time limit, a plugin-driven game library, coexisting with Sunshine and
Apollo, a one-command Linux install, controller speaker and haptics from Linux hosts, host power
control from any client, and Omarchy as a supported host — is described in the
[release notes](https://git.unom.io/unom/punktfunk/releases), and its present-day behaviour is in
the matrix.

## Working on now

**Windows host hardening.** The Windows host is the newest large surface in the project and gets the
most attention. Two strands: field hardening of the AMD (AMF) and Intel (QSV) encode paths — both
ship in the installer and both are validated on real hardware, but they see far less use than
NVENC, so their loss-recovery and 10-bit arms are where new reports land; and capture-stall
attribution — the host now carries dedicated instrumentation that says *which* link went quiet when
a stream stutters — the content that stopped being drawn, or the display path that stopped
presenting it — so field reports stop being guesswork.

**Latency under load.** The remaining wins are no longer in the encoder. They are in when a frame is
handed to the display: client-side presentation phase control, keeping a decoded frame from waiting
out a refresh interval it did not need to, and holding the frame rate when the link degrades rather
than de-escalating and staying degraded. The next structural lever is **sub-frame pipelining** —
overlapping encode and transmit inside a single frame via a direct slice path — which matters most
at high resolutions.

**Finishing the clipboard.** Text crosses today from a Windows, macOS, iOS, iPadOS or Android
client to a host whose operator turned the feature on, with images on all but Android. Two pieces are genuinely
unfinished: the **Linux client's** side of the bridge is a stub, so a Linux client offers and
applies nothing; and **file transfer** has a wire format and a host-side policy but no client that
offers files, so a copied file never crosses. See [Clipboard](/docs/clipboard).

**Console parity with the apps.** The web console still cannot run a speed test or set a bitrate —
it shows the bitrate a live session is using and stops there, while the client apps can measure the
link and act on the result.

**Closing the unverified cells.** The matrix's [What is not
verified](/docs/support-matrix#what-is-not-verified) list is a work list, not a disclaimer:
client-drawn cursors on sway/wlroots and Hyprland, gamescope headless capture on the proprietary
NVIDIA driver, touch input from a Windows client, and re-applying a resolution after an Omarchy
theme switch. Most are settled by a single session log on the right machine.

## Next

**Reach beyond the LAN.** Today a client and host have to be on the same network. The intent is NAT
traversal (ICE/STUN/TURN) with a self-hostable relay for when no direct path can be punched, plus
QUIC connection migration so a client roams between Wi-Fi and cellular without dropping the session.
The control plane is already QUIC over UDP, so this is mostly signalling and relay rather than a
protocol change.

**A host that knows who is connecting.** Streaming to several clients at once already ships — each
one gets its own virtual display at its own resolution and refresh rate, so a laptop, a desktop and
a TV can all be connected at the same time. What is missing is *identity*, in two pieces.
*Per-user sessions*: a connecting client picks an identity — an Apple TV profile, say — which maps
to a real account on the host, and that person lands in their own desktop, signed in, without
touching the machine. And *gamescope multi-user isolation*: per-session input and audio, so those
concurrent clients become fully independent desktops rather than several views of one.

**The rest of remote work.** Multi-monitor streaming (the host's several outputs as separate client
windows), virtual-webcam redirection (the client's camera appears as a webcam on the host, so video
calls run on the remote machine), and peer-approved pairing (approve a new device from an
already-paired device's own app, rather than only from the web console). Each is a new side plane
over the QUIC channel that already exists.

**Picture and sound frontier.** End-to-end variable refresh — the host rendering at a variable rate
and the client presenting with tearing control instead of a fixed cadence (the Apple client already
varies its own display link; the host half does not exist). True glass-to-glass latency measured
capture-to-present rather than capture-to-receipt. And object-based spatial audio: capturing game
audio objects on native Windows is blocked by a closed renderer API, but Proton already routes them
through Wine's `ISpatialAudioClient`, where the dynamic objects and their positions are currently
discarded — finishing that, and tunnelling objects plus positions to the client, would give
head-tracked remote spatial audio that no streaming stack does today.

## Not planned, or blocked upstream

- **HDR on Mutter, KWin and wlroots virtual displays.** Those compositors' virtual-monitor
  screencasts are SDR-only upstream, through the GNOME 51 development branch. The host is ready the
  moment that changes. gamescope's virtual output and the GNOME 50+ real-monitor mirror are the two
  Linux routes that do carry HDR today — see [HDR](/docs/hdr) and the
  [matrix](/docs/support-matrix#input-cursor-and-hdr).
- **Hosting on macOS, iOS, tvOS or Android.** Client-only platforms by construction: every host
  entry point fails at compile time. There is no setting that changes this.
- **HEVC 4:4:4 on the AMD encode block.** AMD's VCN never encodes 4:4:4, so there is nothing to
  implement. Intel is a different story and *is* a gap rather than a wall — the VAAPI backend
  simply has no 4:4:4 path yet, and it waits on hardware that advertises a HEVC 4:4:4 encode
  entrypoint to build and validate against. On either vendor, [PyroWave](/docs/pyrowave) already
  carries full chroma today.
- **DualSense voice-coil haptics over Bluetooth client pads.** The controller exposes no audio
  interface over Bluetooth, so the audio-haptics plane is USB-only on the client side — a BT
  DualSense keeps classic rumble. (Hosts stream pad audio on both Windows and Linux; rumble,
  adaptive triggers and the lightbar work everywhere regardless.)
