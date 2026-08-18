---
title: Controller speaker and haptics
description: DualSense voice-coil haptics and the pad's built-in speaker, streamed from the host to the controller in your hands — what to enable, and what "set it to Pro Audio" means on a Linux host.
---

A DualSense is partly an audio device. Its little speaker and its two voice-coil motors — the
actuators that make a PS5 pad feel like sand, rain or a bowstring instead of a buzzing phone —
are all driven by a four-channel audio stream, not by rumble commands. Games that support them
write PCM into "the controller's audio device".

Punktfunk gives that device to the game on the host, captures what the game writes, and streams
it to the controller physically in your hands, on its own low-latency plane. Channels 1–2 are the
pad's speaker, channels 3–4 are the voice coils.

## What you need

- **A DualSense or DualSense Edge plugged in over USB** on the client. Bluetooth pads expose no
  audio interface at all, so they fall back to ordinary rumble — this is a limit of the
  controller, not of Punktfunk.
- On the client, **Controller haptics** is on by default. So is **Controller speaker** on the Linux
  and Windows apps — turn it off in [client settings](/docs/client-settings#input) if you would
  rather all game audio came out of your speakers. On Android the speaker is opt-in.
- On a **Linux host**, a game that speaks DualSense — which in practice means running it under
  **GE-Proton 11-5 or newer**. Stock Proton does not route controller audio.
- On the host, controller audio is on by default (`PUNKTFUNK_PAD_AUDIO`).

Nothing is sent while the pad is quiet, so leaving it on costs nothing.

## "Set the controller audio to Pro Audio" — you don't have to

If you have looked into DualSense haptics on Linux before, you have probably run into this
advice: plug the pad into the Linux box, open your sound settings, find *DualSense wireless
controller (PS5)*, and switch its **Profile** to **Pro Audio**. That advice is real and it is
correct — for a pad plugged directly into the host.

The reason is channel layout. A pad's other profiles present it as a mono speaker, a stereo
headphone jack, or a positioned four-channel "surround" device. Games write their haptics as four
*unpositioned* channels, so on any of those profiles the audio system helpfully re-mixes them into
the speaker pair and the voice-coil channels are folded away. You feel nothing. Pro Audio is the
one profile that hands the four channels through untouched, in order.

**Punktfunk's controller audio device is already in that shape.** It is created as four raw
channels with no re-mixing, which is exactly what Pro Audio produces — so there is nothing to
switch, and no switch to make.

That is also why it looks different in your sound settings. A real pad is a USB sound card, so it
gets a **Profile** dropdown; Punktfunk's is a software device, so it has no card and no dropdown.
Seeing **Wireless Controller** with a volume slider and no profile selector is what a correctly
minted controller-audio device looks like. It is not a sign that something is missing.

## Checking it is working

On the host, one line per pad is logged when the device is created:

```
pad-audio sink minted (Pro Audio shape: 4ch AUX0..AUX3, ch0/1 speaker, ch2/3 coils)
```

and, once a client that can render it connects:

```
pad audio streaming (0xD1, Opus 48 kHz, silence-gated)
```

When a game actually starts driving the actuators, the pad's own driver reports it:

```
DS5 title asserted haptics-select (audio haptics) pad=0
```

That last line is the one that matters: it means a title recognised the controller as an audio
device and switched the pad out of plain rumble. If you see it and still feel nothing, the problem
is downstream — on the client or the pad. If you never see it, the game never found the device.

You can also look at the device directly:

```sh
pactl list sinks | grep -A25 Speaker__sink
```

The line to check is `audio.position = "AUX0,AUX1,AUX2,AUX3"` — four unpositioned channels is the
layout that reaches the voice coils. Anything positioned (`FL,FR,RL,RR`) would not.

## If a game does not find it

Games identify the controller's audio device by name and by USB ids, and different titles check
different things. GE-Proton has several routes to the pad, and a couple of them are opt-in per
game. Add these as launch options if a title is not cooperating:

```
PROTON_DUALSENSE_HAPTICS_PREFER_NON_EVENT=1 %command%
```

This forces GE onto its most direct route — it opens Punktfunk's controller-audio device by name
and writes the four channels straight into it, with no re-mixing anywhere in between. It is the
first thing to try.

Some titles additionally want:

```
PROTON_SONY_WINDOWS_DEVICE_NAMES=1 PROTON_KEEP_SONY_AUDIO_ENDPOINT_VISIBLE=1 %command%
```

and *Death Stranding Director's Cut* has its own:

```
PROTON_DUALSENSE_SPLIT_AUDIO=1 %command%
```

To see which route GE took, launch the game with `WINEDEBUG=+pulse` and look for a line beginning
`Routing DualSense`. It names the device it chose and how it opened it.

## On a Linux client, the pad's own profile matters too

Everything above is about the host, where the controller-audio device is one Punktfunk mints. On a
Linux **client** the pad is real, and the same channel-layout problem shows up from the other side:
the voice coils are physically channels 3 and 4 of the controller's USB sound card, and a
controller almost never presents as a four-channel device on its own. Depending on your distribution
it appears as a stereo output, or as a mono *Speaker* plus a stereo *Headphones* pair. Playing into
any of those puts the haptics in the headphone jack and folds the coil channels away — audio that
looks perfectly healthy, felt as nothing at all.

**Punktfunk handles this for you.** When it needs the coils and the pad is not already presenting
four channels, it switches the controller's card to **Pro Audio** for the length of the session and
puts your setting back afterwards. You will see the profile change in your sound settings while you
are streaming; that is expected. It is never saved as the card's remembered profile.

If you would rather manage the card yourself, set `PUNKTFUNK_PAD_AUDIO_PROFILE=0` on the client. Then
Punktfunk uses a four-channel profile if you have already selected one and logs what it needs if you
have not.

Many systems never reach the switch at all. On **SteamOS** a DualSense already exposes its four
channels behind a combined speaker-and-haptics output, and Punktfunk finds them there. That is a
Valve addition, though, not something every up-to-date system has: `alsa-ucm-conf` upstream — and
so Fedora, Bazzite and Arch — describes the pad as a *mono speaker plus stereo headphones* and
nothing else, which is precisely the shape that folds the coils away. The switch is the fallback
for those. **If you run the client as a Flatpak**, your audio manager may not let a sandboxed app
change a card's profile; if the log says so, switch the controller to Pro Audio yourself, which is
the same fix.

Punktfunk's **host** packages (rpm, deb, Arch, and the Bazzite sysext) close that gap at the
source: they install a small ALSA profile for the DualSense that adds the combined
speaker-and-haptics output SteamOS has, and give it priority over the mono one. It adds files
rather than replacing any your distribution owns, so it upgrades cleanly and can be removed by
uninstalling Punktfunk. A pad plugged into the host then presents four channels on its own, with
no profile switching by anyone — and, because the lone mono output stops existing, games that
crashed when they opened it stop crashing. A card reads its profile once, when it appears, so
replug the pad after installing (or restart PipeWire) rather than expecting a pad that was
already plugged in to pick it up.

### Checking the client side without a host

The client can test the whole path on its own — no host, no game, no pairing. Plug in the
DualSense and run:

```sh
punktfunk-session --pad-audio-test
```

It prints every DualSense object it can see in your audio graph, says which one it chose, and then
plays a tone into the voice coils for three seconds. **If the pad buzzes, the client side is
working** and any remaining silence is coming from the host or the game. Add `--speaker` to test
the pad's speaker instead, and `--seconds N` for a longer run.

On the Steam Deck and other flatpak installs, run it inside the sandbox:

```sh
flatpak run --command=punktfunk-session io.unom.Punktfunk --pad-audio-test
```

### Why the speaker needs more than routing

The controller's speaker and its headphone jack **share a channel**. Channel 1 of the pad's audio
device is the headphone jack's right channel *and* the built-in speaker, and the controller decides
which one actually sounds. It powers up pointing at the jack — so with nothing plugged in, a
perfectly routed speaker stream is heard by nobody.

Punktfunk points the pad at its own speaker when **Controller speaker** is on. The voice coils are
different channels and are not affected by that choice, which is why haptics work as soon as the
audio is routed correctly and the speaker needs this extra step. A game that drives the pad's audio
settings itself still overrides it. If your pad's speaker stays quiet, `PUNKTFUNK_PAD_SPEAKER_PATH`
and `PUNKTFUNK_PAD_SPEAKER_VOLUME` let you bisect it without a rebuild.

## Known limits

- **Bluetooth client pads get rumble, not haptics.** No audio interface exists over BT.
- **Titles that match the controller by container ID** — a Windows notion of "these devices are
  the same physical thing" — will not recognise the pairing on a Linux host, because the virtual
  pad has no USB device behind it to derive one from. Titles that match by name or by USB ids are
  unaffected, which is most of them.
- **A pad plugged into the host itself can steal the audio.** If a real DualSense is connected to
  the host while you are streaming to a different one, some titles will find the local pad's sound
  card first. Unplug it, or stream from a host that has no pad attached.
- **The Pro Audio switch on a Linux client renames the pad's microphone too.** Switching a sound
  card's profile re-creates all of its inputs and outputs, so if you had picked the DualSense's own
  microphone as your [mic](/docs/client-settings#audio), that session falls back to your default
  one. Pick a different microphone, or set `PUNKTFUNK_PAD_AUDIO_PROFILE=0` and select a
  four-channel profile on the card yourself.
- **A client killed mid-stream leaves the pad on Pro Audio.** The profile is restored when a
  session ends normally and is never written to your saved settings, so anything that reloads the
  card — unplugging it, logging out, a reboot — brings your own profile back.
