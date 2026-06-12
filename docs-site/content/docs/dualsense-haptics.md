---
title: "DualSense Haptics"
description: "Feasibility and scoping for audio-driven DualSense haptics."
---


**Status: scoped, NO-GO for now (deferred).** Advanced voice-coil haptics on the DualSense are
driven by the controller's **USB audio interface** (4-channel surround, the back two channels carry
the haptic waveform), *not* by HID reports. Emulating that on a Linux host and faithfully replaying
it on the Apple client both hit hard walls, and the supply of software that actually *emits* these
haptics on a Linux host is essentially zero. We defer the audio-haptics feature and instead land the
parts of "really supporting the DualSense" that *are* reachable: **adaptive triggers (HID) and
two-motor rumble.**

(Grounded in a 4-agent feasibility read — host USB-gadget viability, DualSense audio descriptors,
Linux game demand, Apple client render path — 2026-06-10.)

## The one distinction that decides everything

| Feature | How it's driven | Reachable for us? |
|---|---|---|
| Basic rumble (2 motors) | HID output report `0x02`, bytes 3–4 | **Yes** — already parsed; client already has `nextRumble()` |
| **Adaptive triggers** (L2/R2 resistance) | HID output report `0x02`, bytes 11–22 / 22–33 | **Yes** — already parsed in `dualsense.rs`; just needs the `0xCD` back-channel + client render |
| **Advanced haptics** (voice-coil actuators) | **USB *audio* interface** — 4-ch, back 2 channels = haptic PCM | **No (for now)** — see the three walls below |

The UHID DualSense we already built is **HID-only**. It cannot present the DualSense's *audio*
interface, so it structurally cannot carry advanced haptics. That's not a bug in our implementation —
it's the wrong transport for this signal.

## The three walls (any one is fatal on its own)

### Wall 1 — Host capture needs a kernel rebuild
To *capture* haptic audio a game emits, the host must present a virtual device that owns the
DualSense audio interface. The standard way is a composite USB gadget (`configfs` + `f_hid` +
`f_uac2`) bound to a software UDC (`dummy_hcd`).

- ✅ Present & enabled on this box: `CONFIG_USB_CONFIGFS`, `CONFIG_USB_CONFIGFS_F_HID`,
  `CONFIG_USB_CONFIGFS_F_UAC2`, plus `libcomposite`/`usb_f_hid`/`usb_f_uac2`/`u_audio` modules.
- ❌ **Blocker:** `# CONFIG_USB_DUMMY_HCD is not set` in `/boot/config-7.0.0-22-generic`. No
  `dummy_hcd.ko`, no `/sys/class/udc/`. **No UDC → nothing to bind the gadget to.** Requires a
  custom kernel build to enable `CONFIG_USB_DUMMY_HCD=m`, plus root for module-load/configfs.

A lighter alternative exists — a **virtual PipeWire/ALSA sink renamed as the DualSense** (this is how
the working Linux setups capture the back-2-channels today, via WirePlumber rules). It skips the
kernel rebuild, but is gated by the same Wall 2 below, and games' audio-device detection is
hardcoded per-title so it's fragile.

### Wall 2 — Almost nothing on a Linux host emits these haptics
This is the decisive one. The *supply* that would feed our capture barely exists:

- **Steam Input (Linux):** no official advanced-haptics support (open feature request as of 2026).
- **Sony's `hid-playstation` kernel driver:** explicitly does **not** expose VCM haptics or adaptive
  triggers — basic rumble only.
- **RPCS3:** treats the DualSense as a generic pad; no advanced haptics.
- **Native Linux games:** effectively **zero** with advanced haptics.
- **The only working path** is a handful of Proton titles (FF7 Remake, Ghostwire, Deathloop, Animal
  Well, Stellar Blade) via ClearlyClaire's *custom Wine patches*, **USB-only**, Steam Input
  disabled, forced into a 4.0-surround profile, device renamed to match Windows. ~5–10 games total.
- Bluetooth can't carry it on Linux *or* Windows (Sony's proprietary A2DP repurposing isn't exposed).

A host-side capture feature is only as useful as the software willing to drive it. On Linux that set
is a niche-of-a-niche.

### Wall 3 — The Apple client can't faithfully replay it
Even with a captured waveform, the primary client (macOS/iOS) can't render it well:

- macOS GameController exposes the DualSense as a **basic gamepad** — no voice-coil / adaptive-trigger
  access. Those are PS5-only in Apple's stack.
- CoreHaptics is **discrete, pattern-based** (`CHHapticPattern` events, ≤30 s), **not** a PCM
  streaming sink. Converting a streamed haptic waveform to patterns is lossy — it throws away exactly
  the fidelity that makes voice-coil haptics worth having.
- There is **no public macOS API** to route CoreAudio to the DualSense's channels 3–4. Doing it
  anyway means private/reverse-engineered APIs that break across OS updates.

## What we *can* ship instead ("really supporting the DualSense" minus audio haptics)

The HID DualSense we built is the foundation, and the high-value parts are within reach:

1. **Adaptive triggers — GO.** `dualsense.rs` already parses the L2/R2 trigger effects out of HID
   output report `0x02`. Finishing this is the paused HID work: route them over the `0xCD`
   HID-output back-channel and render on the client. This delivers the headline "DualSense feel"
   (trigger resistance/weapon tension) for any source that emits it — and it's pure HID, no audio
   interface, no kernel rebuild.
2. **Two-motor rumble — already done.** Parsed host-side; the Apple client already has
   `nextRumble()`. Wire it to `GCDeviceHaptics`/`CHHapticEngine` as discrete patterns (API-clean,
   no private APIs).
3. **LED / player-LED / touchpad / motion** — already parsed; finish the `0xCC`/`0xCD` routing.

This is the resume-able HID DualSense Phase C/D/E work — it stands on its own and was never blocked.

## Conditions for a future GO on audio haptics

Revisit if **all three** change:

- A real DualSense is available on the dev box to capture an authoritative `lsusb -v` + the exact
  UAC channel/sample-rate/format layout (today: undocumented, would need reverse-engineering).
- The host target gains a UDC (custom kernel with `dummy_hcd`, or real hardware OTG) **or** we accept
  the PipeWire-renamed-sink path *and* the title set that emits haptics on Linux grows beyond the
  Proton-patch niche.
- The client target shifts to one that can render PCM haptics (a Linux/Windows client with direct
  CoreAudio-style channel access, or a future Apple API) — or we accept lossy pattern conversion.

Until then the cost/benefit is upside-down: three hard subsystems (kernel, USB gadget, audio
routing) to serve ~5–10 Proton titles, rendered lossily on the one client we ship.

## Recommendation

**Defer audio-driven advanced haptics. Land adaptive triggers (HID) + rumble instead** — that's the
reachable 80% of "really supporting the DualSense," needs no kernel work, and the parsing is already
written. Keep this doc as the down payment for the audio-haptics feature whenever the three
conditions above are met.
