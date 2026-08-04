# App Review notes

## The core problem, stated plainly

Punktfunk is the client half of a two-part system. Without a reachable host it shows a host list, a
pairing sheet, and settings — and nothing else. There is **no demo or offline mode in a release
build**: the mock-data screens in `Sources/PunktfunkClient/Screenshots/` are wrapped in `#if DEBUG`
and are compiled out of anything you ship. A reviewer who launches the App Store build with no host
on their network sees an empty "On this network" list.

Guideline 2.1 requires you to supply whatever is needed to fully exercise the app. So you must
attach **one** of:

- **(a) A reachable demo host.** Best outcome — the reviewer sees the real thing. Requires a host
  exposed to the internet with its UDP ports forwarded, plus a pairing PIN in the notes. The client
  can add a host by IP or hostname, so mDNS discovery is not required for this path.
- **(b) A demo video.** Apple accepts this for hardware- or setup-dependent apps. Less good: a
  reviewer who cannot reproduce is a reviewer who can reject on something unrelated.

**Attach (a) if you can keep a host up for the review window; (b) is the fallback.** Whichever you
pick, fill in the placeholders before submitting — the template assumes (a) and marks the spots.

> **⚠ Decide before submitting:** if you go with (b), replace the "CONNECTING TO OUR DEMO HOST"
> section with the video URL and say explicitly that no host can be provided.

---

## Notes template — paste into App Store Connect

The App Review Information "Notes" field caps at **4000 characters**. The block below is **3919**,
and filling the five placeholders in shortens it further (the literal `[[FILL IN: …]]` text is
longer than the values that replace it). If you add to it, re-check the count — an over-long note
is silently truncated, and what gets cut is the end, where the privacy and entitlement answers
live.

```
WHAT THIS APP IS

Punktfunk is a low-latency game- and desktop-streaming client. It streams from a "host" the user
installs on their own gaming PC (Linux, or Windows 11 22H2+), over their own network. The host is
separate open-source software we publish at https://git.unom.io/unom/punktfunk; it is not sold,
and this app has no purchases.

This app is the client half only: it renders video and audio from the user's own machine and
sends input back. There is no content library and no server of ours in a session.

IMPORTANT: THIS APP NEEDS A HOST

With no reachable host, the app can only show its host list, the pairing screen and settings --
inherent to what it is, not an incomplete build. We have provided a live host for review.

CONNECTING TO OUR DEMO HOST

1. Launch Punktfunk. The main screen lists hosts on the local network. Ours is not on yours, so
   add it by hand: "+" (top right) then "Add host"; on Apple TV, "Add host" on the main screen.
2. Enter:  Host: [[FILL IN: hostname or IP]]   Port: [[FILL IN: port, default 47998]]
   Name it anything, then confirm.
3. The app connects and asks for a pairing PIN. Enter:  [[FILL IN: PIN]]
   A one-time SPAKE2 pairing; afterwards the device is remembered and needs no PIN.
4. The host's game library appears as a grid. Select any title to stream; video and audio start
   within a few seconds.
5. While streaming: stats overlay = Ctrl+Alt+Shift+S (or three-finger tap on iOS/iPadOS); release
   mouse = Cmd+Esc or Ctrl+Alt+Shift+Q; disconnect = Ctrl+Alt+Shift+D.
6. Settings (gear) covers decoder, bitrate, HDR, audio, controllers and profiles; the per-host
   "Speed test" suggests a bitrate for the link.

The host stays reachable throughout review. If you cannot reach it, please contact
[[FILL IN: contact email]] and we will restore it promptly.

WHY THE APP ASKS FOR WHAT IT ASKS FOR

- Local Network: finds hosts via Bonjour (_punktfunk._udp) and connects to them -- the app's
  entire purpose.
- Microphone (optional, off by default): audio goes to the user's own paired host, appearing
  there as a virtual microphone for voice chat. Never recorded, never sent to us.
- networking.multicast: sends the Wake-on-LAN magic packet, which must go to a broadcast address:
  a sleeping PC has no ARP entry, so unicast cannot reach it. Used for nothing else.
- device.usb / device.bluetooth (macOS): the GameController framework reaches wired controllers
  through IOHIDLibUserClient and wireless ones through startWirelessControllerDiscovery. USB also
  drives DualSense rumble, which CoreHaptics will not. Without these, no controller input.
- network.server (macOS): the app is outbound-only, but the App Sandbox gates bind() itself. Our
  QUIC endpoint and UDP socket each bind a local port to receive host-to-client datagrams;
  without this, no video, audio or rumble arrives.
- UIBackgroundModes "audio" (iPhone/iPad): a session carries real, audible audio from the host,
  and this keeps it alive if the user steps away briefly. Backgrounded, video decoding stops, only
  the real audio keeps rendering, and a bounded timer disconnects automatically. We never play
  silence to stay alive, nor use the mode outside an audible session.

REGARDING BUILD 0.4.2 (3384)

That build was rejected under 2.4.5(i) for a temporary-exception entitlement
(mach-lookup.global-name, com.apple.audioanalyticsd), added on a mistaken belief about CoreHaptics
rumble under the App Sandbox. We have since verified rumble works without it; this build carries
no temporary exception.

ACCOUNTS, PURCHASES, DATA

No account, no sign-in, no in-app purchase. The app collects no personal data: no analytics,
tracking, advertising or crash-reporting SDKs, and no connection to any server of ours during a
session. Device identity is a keychain keypair used only to authenticate to the user's own host.

Privacy policy: [[FILL IN: https://punktfunk.unom.io/legal/privacy]]
```

---

## Before you submit — checklist

- [ ] Fill every `[[FILL IN: …]]` placeholder. There are five.
- [ ] Confirm the demo host is reachable **from outside your own network** — test it on cellular,
      not on the LAN it lives on. This is the failure mode that wastes a review cycle.
- [ ] Confirm the pairing PIN in the notes is the one the host will actually accept during the
      review window, and that pairing is left open (it is on-demand in the web console).
- [ ] Put at least one launchable title in the demo host's library. An empty grid after a
      successful pairing looks like a broken app.
- [ ] If submitting tvOS, verify the whole flow is reachable with the **Siri Remote alone**. A
      reviewer will not have a controller paired, and "requires an accessory to navigate" is a
      tvOS rejection.
- [ ] Attach the demo video as a URL in the notes if you are going the (b) route.

## Separately worth checking: the privacy manifest

There is **no `PrivacyInfo.xcprivacy`** anywhere in `clients/apple`. The app does use
`UserDefaults` (`HostStore` reads the `group.io.unom.punktfunk` suite), and `UserDefaults` is one of
Apple's "required reason" APIs, which are expected to be declared in a privacy manifest. Apps
missing a declaration typically get an automated **ITMS-91053** notice on upload.

This is adjacent to the copy work rather than part of it, so nothing has been changed here — but it
is worth adding a manifest declaring `NSPrivacyAccessedAPICategoryUserDefaults` with reason code
`CA92.1` (access to an app group container) and `NSPrivacyTracking` set to `false`, before the next
submission. Confirm the current reason codes against Apple's documentation rather than taking the
code above on trust; the list has changed since it was introduced.
