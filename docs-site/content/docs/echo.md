---
title: Why do I hear myself
description: Echo while streaming — the four places it can come from (client speakers, Windows monitoring loops, host speakers, virtual mixers) and how to stop each one.
---

You talk into your device's microphone and hear your own voice come back a beat later. The echo
is almost never "in the stream" itself — it's a loop in one of four well-known places. Work
through them in order; the first two cover nearly every report.

## Your device's speakers

If the stream's audio plays out of the **speakers of the device you're streaming on** — a
phone, tablet or laptop without headphones — its microphone picks that sound back up and sends
it to the host along with your voice. Everyone in your voice chat hears the game twice, and you
hear yourself whenever anything routes the mic back.

**Fix: use headphones on the device you're streaming on.** Clients also try to cancel it for
you: **Echo cancellation** in [client settings](/docs/client-settings#audio) is on by default on
Linux, Windows, the console home, the Apple apps and Android, and hands the microphone to the
system's own canceller. How much it removes depends on the device — a laptop with a good array
mic can clear it entirely, a cheap USB mic next to a speaker can't — so headphones remain the
reliable fix everywhere.

If you just need to stop talking for a moment, **Ctrl+Alt+Shift+V** mutes the microphone without
leaving the stream; see [Input](/docs/input#getting-your-input-back).

## "Listen to this device" and app monitoring (Windows hosts)

Windows can play a microphone straight out of the speakers. If **Listen to this device** is
ticked for the Punktfunk mic — a *Steam Streaming Microphone*-class device on current hosts,
*CABLE Output* on older ones — your voice plays on the host's output, which the stream then
captures and sends right back to you.

Open **Sound settings → More sound settings → Recording**, double-click the Punktfunk mic, and
on the **Listen** tab untick *Listen to this device*.

The same loop hides in apps: **Discord's** *Mic Test* / input monitoring, **OBS's** *Monitor
audio* on a mic source, and similar monitoring features in other tools all play your mic into
the host's output. Turn the monitoring off rather than the mic.

## The host's own speakers

If you set `PUNKTFUNK_HOST_AUDIO` (Windows) so the stream's sound also plays in the room, and
you're streaming **from that same room**, your device's mic hears the host's speakers. Remove
the setting while you stream from nearby, or turn the host's volume down.

## Virtual mixers (VoiceMeeter and friends)

VoiceMeeter's virtual devices all share one internal mixer. Older Punktfunk hosts could pick one
VoiceMeeter strip as the microphone target and *another* as the audio capture — a feedback loop
with no acoustics involved at all. Current hosts refuse to capture VoiceMeeter or other virtual
endpoints for desktop audio, so this fixes itself with an update. If you route audio through
VoiceMeeter on purpose, make sure no strip that hears the Punktfunk mic feeds the output being
streamed.

## What the host log can tell you

While your microphone is in use, the host writes a **`mic uplink health`** line to its log every
30 seconds (web console → **Logs**). It shows how much of your voice is buffered on the host
(`depth_ms` vs `target_ms`), how much the network lost (`gaps`, `concealed`), and how steadily
your client is delivering audio (`cadence_ms`). It won't point at an echo loop directly — echo
is a routing problem, not a network one — but if your voice also sounds choppy or delayed,
include that line in a bug report. The startup log also names exactly which devices the host
picked for the microphone and for audio capture, which is the quickest way to spot a monitoring
loop like the ones above.
