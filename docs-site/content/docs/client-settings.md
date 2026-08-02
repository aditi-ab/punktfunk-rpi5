---
title: Client settings
description: Every setting a Punktfunk client stores — what it does, what it defaults to, and which of them the host can overrule.
---

The host has [its own settings reference](/docs/configuration). This page is the other half: the
settings each **client** keeps, which together decide what a session looks like.

Most of them are a *request*. The client asks, the host answers, and the answer comes back in the
handshake — so a setting the host can't honor is usually a quiet downgrade rather than an error.

## Where the settings live

The Linux, Windows, Mac, iPhone/iPad and Android apps group settings the same way — **General**,
**Display**, **Input**, **Audio**, **Controllers** — under *Preferences* on Linux and *Settings*
elsewhere. The Apple TV app shows one scrolling list instead, and so does any client's settings
screen reached with a controller. A controller-driven launch (Steam Deck Gaming Mode) opens the
client's **console home**, whose settings screen is one steppable list; the Decky plugin's Settings
tab covers the same store in the same groups and the same order, as a left rail of categories the
way SteamOS's own Settings looks. The console home is part of the
client — it is not the host's
[web console](/docs/web-console).

Linux stores them in `~/.config/punktfunk/client-gtk-settings.json`, the same file the Decky plugin
writes, so a change in either shows up in the other. Windows uses
`%APPDATA%\punktfunk\client-windows-settings.json`; the Apple and Android apps use their own stores.

Changes apply to the **next** session — a running stream keeps what it started with. (*Match window*
is the exception in effect, not in reading: it too is read at connect, but once a session is running
with it on, every window resize renegotiates the mode.)

Not every client offers every setting, and the wording on screen varies a little between them — the
names below are the ones the Linux app uses. The differences that matter are noted per setting.

## Video

**Resolution** — *default: Native display.* The host builds a virtual display at exactly this size
and streams it; nothing is scaled. Native resolves at connect to the mode of the display your window
is on. The Apple app instead stores an explicit size (1920 × 1080 out of the box): on iPhone, iPad
and Mac a **Use this display's mode** button fills in what you're looking at, and the Apple TV app
picks a combined **Stream mode** preset instead ("This TV (native)", 720p, 1080p or 4K at 60 Hz). If
the host has been pinned to stream a *real* monitor rather than make one, your request is declined
and your client scales what it gets — see
[Virtual displays](/docs/virtual-displays#stream-a-real-monitor-instead).

**Match window** — *default: off.* The stream mode follows your window instead, and each resize
renegotiates the host's display and encoder, so a windowed session stays pixel-exact. Fullscreen
degenerates to the display's native mode. Offered by the Linux, Windows, Mac, iPhone/iPad, console
home and Decky screens (on Decky it sits in the Resolution picker, and Gaming-Mode streams are
always fullscreen, so it lands on native); not by Android.

**Refresh rate** — *default: Native*, the refresh of the display your window is on. The Apple app
stores an explicit rate (60 Hz by default): iPhone and iPad offer the rates the device can display,
on a Mac you type one in, and on Apple TV the rate rides along with the Stream mode preset above.

**Bitrate** — *default: Automatic.* For H.264, HEVC and AV1, Automatic means the host's own default,
**20 Mbps**, and it turns on two things an explicit rate switches off: adaptive bitrate, and a short
link-capacity probe about two seconds in that measures what your link really carries and lets the
rate climb past 20 Mbps. An explicit rate is fixed for the session, and clamped by the host to
**500 kbps – 8 Gbps**. A host card's menu has a **Test network speed…** entry that measures your link
and suggests a value.

PyroWave is the exception: it has no useful low-rate regime, so its Automatic rate is a fixed
per-pixel budget for the negotiated mode (hundreds of Mbps), and both adaptive bitrate and the
capacity probe stay off for the whole session.

**Render scale** — *default: Native (1×).* The host renders and encodes at your chosen mode
multiplied by this, and your device resamples the result to its window. Above 1× supersamples for
sharpness, at more bandwidth *and* more decode work; below 1× is lighter on both the host and the
link. The stops run 0.5× to 4×. The result is floored to an even size and capped per axis at
4096 px for H.264, 8192 px otherwise. Offered everywhere except the console home's list.

**Video codec** — *default: Automatic.* A soft preference: the host emits your choice when it can
also produce it, otherwise the best codec you both speak, in the order HEVC → AV1 → H.264.
**PyroWave** is never auto-picked — pick it explicitly on Linux, Windows, the console home, Decky, or
an Apple device whose decode probe passes; anywhere else it isn't offered, and asking for it lands on
that same order. See [PyroWave](/docs/pyrowave). The Android and Apple apps hide AV1 unless the
device has a hardware AV1 decoder; Android never offers PyroWave.

**10-bit HDR** — *default: on.* Off means "never send me 10-bit", and the host then never upgrades.
On, the stream goes 10-bit BT.2020 PQ only when the host has HDR content *and* the encoder can do
10-bit. Android disables the toggle, and never advertises HDR, on a panel that can't present HDR10.
Full detail: [HDR](/docs/hdr).

**Full chroma (4:4:4)** — *default: off.* Crisp small text and thin lines, at more bandwidth. It
needs HEVC or PyroWave, the host's own 4:4:4 policy left on, a capture path that delivers full
chroma, and a GPU that can encode it; if any gate fails the host says 4:2:0 before your decoder is
built. **Today only the Apple app actually advertises 4:4:4**, and only when its hardware decode
probe passes — the Linux and Windows apps store the toggle but their session doesn't advertise the
capability yet, so it has no effect there. The console home and Decky offer the toggle; Android
doesn't.

**Host compositor** — *default: Automatic.* Which backend a **Linux** host uses to drive the virtual
output. Advisory: a host without that backend quietly auto-detects instead.

## Audio

**Audio channels** — *default: Stereo.* You can ask for **5.1** or **7.1**; anything else is read as
stereo. The count the host will really send comes back in the handshake, and your client builds its
decoder from *that*, never from the request. What surround means differs by host: a **Linux** host
claims a sink advertising exactly that many channels, so applications produce real surround, while a
**Windows** host loopback-captures your current output endpoint and lets Windows convert it — so 5.1
from a stereo endpoint is an upmix, not new channels. Offered everywhere.

**Microphone** — *default: off on Linux, Windows, Android, the console home and Decky; on in the
Apple app.* Sends this device's microphone to the host's virtual mic. On Linux and Windows the
row is spelled *Stream microphone*, and **Ctrl+Alt+Shift+V** mutes it mid-stream without ending
anything — see [Muting your microphone](/docs/input#muting-your-microphone).

**Echo cancellation** — *default: on.* Stops the host's audio, playing out of this device's
speakers, from being picked up by the microphone and sent straight back. It hands the microphone
to the system's own canceller rather than doing the work itself: on **Linux** that means capturing
from an echo-cancelled PipeWire source when your desktop provides one, on **Windows** asking WASAPI
for the Communications stream category so the endpoint's processing engages, and on **Apple** and
**Android** the platform's voice-processing mode. Turn it off if your microphone already runs its
own processing, or if the canceller makes your voice sound thin. The row sits under the microphone
toggle and greys out while the microphone is off. Offered by the Linux, Windows, Apple, Android,
console-home and Decky clients. What it can and can't fix is in
[Why do I hear myself](/docs/echo).

**Speaker** and **Microphone** device pickers — *default: System default.* Which endpoint stream
audio plays out of, and which input feeds the uplink. Only the Linux app (PipeWire nodes), the
**Mac** app (which also has a microphone *channel* picker) and **Decky** have these — iPhone, iPad,
Apple TV, Android and the console home have none, and the Windows app has none and ignores a stored
speaker choice. On Linux, a device that has since disappeared keeps a "(not detected)" entry rather
than silently snapping back to the default; the Mac shows it as "Unavailable device" and Decky as
"(not connected)". Decky reads the endpoint list from the client's session binary, so a client
older than the two-binary split leaves these pickers on Automatic.

## Input

Touch modes, mouse modes and the in-stream chords have their own page: [Input](/docs/input). Five
more settings are worth naming here.

**Forward controllers** — *default: on*, on every client. Off, the controllers connected to *this*
device are not sent to the host at all. That is what you want when your controller already reaches
the host by some other route — [USB passthrough](/docs/automation#recipe-full-controller-passthrough-virtualhere)
such as VirtualHere, or simply a pad plugged into the host itself. Leaving forwarding on in that
situation hands the host two controllers for one pair of hands, and games read both: a stick drifts
because the second pad is centred, or a menu takes every input twice.

On Linux and Windows it does more than stay quiet. Opening a controller is what *claims* it — the
client's SDL takes the device node — and a claimed device is one a passthrough tool cannot bind. So
with this off the session never opens the controller at all, which is precisely what leaves it free
for VirtualHere to hand over. The consequence to know: the
[controller escape chord](/docs/input#leaving-with-a-controller) is read off forwarded pads, so it is
unavailable on those two while this is off — leave a stream with the keyboard chord or the client's
own UI. The Apple and Android apps claim nothing, so their chords keep working either way; the
Android app does stop its DualSense and Steam Controller 2 USB captures, which *do* claim the
device.

The rows below it — which pad, and what type — have nothing to act on while this is off, and every
client greys them out to say so.

**Gamepad type** (*Controller type* on Apple, Android and the console home) — *default: Automatic*,
which matches each physical controller. The pickers offer Xbox 360, Xbox One, DualSense and
DualShock 4 everywhere, plus Steam Deck on Linux, Android, the console home and Decky. Your client
declares a type per pad as it connects — Automatic declares what that controller really is, an
explicit choice declares your choice — and the host builds each virtual pad from that. A type the
host has no backend for degrades to an Xbox 360 pad rather than failing: Xbox One on a Windows host,
for instance, or any Sony pad on a Linux host that can't open `/dev/uhid`.

**Forwarded controller** (*Use controller* on Apple and the console home) — *default: Automatic*,
which forwards *every* connected controller, each as its own player, on Linux, Windows, Apple and the
console home. Pinning one restricts the session to that controller alone — single-player. The Android
app has no such picker.

**Capture system shortcuts** — *default: on.* Offered by the Linux and Windows apps, the console home
and Decky; Windows spells the row out as *Capture system shortcuts (Alt+Tab, Win, …)*. On a Deck it
matters only for a keyboard you attached yourself, for the reason the paragraph below gives: Gaming
Mode is gamescope, which has nothing to hold back. On, Alt+Tab and the Windows key
(Super on Linux) reach the host while the stream has input captured. Off, they act on this machine
instead — what you want when the stream shares a screen with local work. Either way the chords come
back the moment you release capture with **Ctrl+Alt+Shift+Q**, the window loses focus, or the stream
ends, and [Desktop mouse mode](/docs/input#mouse-modes) never takes them at all. Leaving this on does
mean **Ctrl+Alt+Shift+Q is your way out** of a captured stream, since Alt+Tab no longer is.

On Linux this needs a compositor that supports keyboard-shortcuts-inhibit — KDE Plasma, GNOME and
the wlroots compositors all do, and X11 sessions grab the keyboard directly. Under
[gamescope](/docs/gamescope) there is nothing to inhibit: it hands the session everything already.

**Invert scroll direction** — *default: off*, i.e. the host scrolls the way this machine does.

## Behavior

**Auto-wake on connect** — *default: on.* Connecting to a saved host that looks offline sends
Wake-on-LAN and waits for it to boot — only for a host whose MAC address this client has already
learned. Turn it off for hosts you reach over a VPN, where "offline" usually means "not reachable by
broadcast" and the wake only adds a delay. The Linux, Windows, Apple and Android apps have this
toggle, as do the console home and Decky — and note that the Decky plugin sends a wake of its own
before a stream starts whatever this setting says, so on a Deck it governs the client's connect
rather than the launch. The console home also offers wake as an explicit action on an offline host.
See
[Wake-on-LAN](/docs/wake-on-lan).

**Show game library** — *default: off on Linux and Windows; on in the Apple and Android apps.* Browse
a paired host's games and launch one directly; the Windows app still labels it experimental. The
console home and Decky have the toggle too — on Decky it governs the *client's* screens, since the
plugin's own library browser works either way. See [Game library](/docs/game-library).

**Start streams in fullscreen** — *default: on.* On Linux and Windows, F11 or Alt+Enter leaves
fullscreen live. On a Mac the setting is **Fullscreen while streaming**, and the window comes back
when you return to the host list. The console home and Decky carry the row for the desktop client
that shares the store — a Gaming-Mode launch is fullscreen whatever it says. iPhone, iPad, Apple TV
and Android have no equivalent.

## Overlay

**Statistics overlay** — *default: Normal.* Four tiers — Off, Compact, Normal, Detailed — each a
superset of the one before. This setting only picks the tier a session *starts* at — you can cycle
them live in-stream, with a shortcut that differs by platform. The Apple app additionally lets you
choose which corner the overlay sits in (Top Left, Top Right, Bottom Left, Bottom Right). The Decky
plugin has the tier picker too, in its Settings section. The shortcuts, and every number in the
overlay, are in
[Understanding the stats overlay](/docs/stats).

## Settings that are facts about your device

A few of these describe the machine you're sitting at rather than how you want a host streamed. They
stay global and **cannot be put in a settings profile**:

- **Video decoder** and **GPU** — the decode path and adapter this device uses. Automatic is
  vendor-ordered and falls back on its own; change it only when debugging, and note that
  `PUNKTFUNK_DECODER` overrides it
  ([Configuration](/docs/configuration#client-side-native-clients)). The decoder picker is on Linux,
  Windows, in the console home and in Decky; the GPU picker on Windows, and on Linux and Decky only
  when the machine has more than one adapter — which a Deck doesn't, so the row isn't there. The
  Apple and Android apps have neither.
- **Speaker** and **Microphone** device pickers — this device's audio endpoints.
- **Forwarded controller** — which physical pad is in your hands. The *type* the host creates is a
  preference and can live in a profile; which pad you hold cannot. **Forward controllers** is a
  preference too, and does live in a profile — a work profile can decline to forward what a game
  profile forwards.
- **Auto-wake on connect** and **Show game library** — decisions about this device and this network,
  not about how a given host is streamed.

One switch you might expect here isn't in Settings at all: **Share clipboard** lives in a saved
host's own edit sheet, because handing a machine your clipboard is a decision about that one host —
see [Shared clipboard](/docs/clipboard).

Everything else on this page can be overridden per profile and bound to a host; the rows above are
exactly [what a profile can't change](/docs/profiles-and-links#what-a-profile-cant-change).

## When the client and the host disagree

| You ask for | What the host does |
|---|---|
| Resolution and refresh | Builds a display at exactly that mode. A host pinned to a real monitor keeps that monitor's resolution and you scale locally. A size the encoder can't take — odd, or past the codec's per-axis limit — fails the connect rather than being quietly changed. |
| A bitrate | Clamps it to 500 kbps – 8 Gbps, or uses its 20 Mbps default for Automatic (a per-pixel budget for Automatic PyroWave). |
| A codec | Honors it when it can encode it, else the best shared codec in the order HEVC → AV1 → H.264. |
| 10-bit HDR | Upgrades only for HDR content on an encoder that can do 10-bit; otherwise 8-bit SDR. |
| 4:4:4 chroma | Sends it only when every gate passes; otherwise 4:2:0. |
| A channel count | Normalizes it to 2, 6 or 8. |
| A gamepad type | Uses it as the session default; an unsupported type becomes an Xbox 360 pad. |
| A compositor | Treats it as advisory and auto-detects when it isn't available. |

Every one of those answers arrives before your decoder and speakers are set up, so what you see and
hear is built from what the host really sent — never from what you asked for.
