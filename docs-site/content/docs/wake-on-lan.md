---
title: Wake-on-LAN
description: How Punktfunk clients wake a sleeping host — what has to have happened first, what a click does, the punktfunk wake command, and how to arm the machine so the packet actually lands.
---

A host that is asleep answers nothing. Punktfunk works around that by remembering the host's network
card address — its **MAC address** — while the host is awake, and sending it a **magic packet**, the
standard Wake-on-LAN wake-up datagram, when you later ask to connect.

The Linux, Windows, Apple and Android apps do this by default. There is nothing to enable in
Punktfunk itself. The work is on the *machine*: its BIOS/UEFI and its network card have to be armed
to wake, and that is where Wake-on-LAN usually fails. Jump to
[Arming the machine](#arming-the-machine) if that is what you are here for.

## How it works

While it is running, the host advertises itself on the local network over mDNS. One of the things it
publishes is `mac` — the MAC address of the network card that carries the IP clients reach it on
first, then any other non-loopback cards as fallbacks, at most four.

Each app stores those addresses on its **saved host** record. The Linux, Windows and Android apps
refresh them whenever they see the host advertise; the Apple app refreshes them when you save the
host and on every connect. When the host later sleeps it stops advertising entirely, but the client
still has the addresses on disk, so it can still aim a packet at the machine.

That ordering is the whole prerequisite:

> **The client must have seen the host awake at least once**, on a network where the host's mDNS
> advert reached it. Until then no address is known and there is nothing to wake with — the client
> says so rather than pretending. On every client but the Linux one you can also type the MAC in by
> hand; see the table below.

The packet goes to every local interface's subnet broadcast address *and* to `255.255.255.255`, on
UDP ports 9 and 7, repeated three times, plus a unicast to the host's last known address. That
spread is deliberate: a sleeping machine has no ARP entry, so a plain unicast cannot find it.

Neither the advert nor a magic packet is authenticated. That is fine here — a wrong address only
makes the wake fail, and the host's certificate fingerprint still gates the actual connection. See
[Security](/docs/security).

## Waking from a client

**Auto-wake on connect** is a client setting, and it is **on by default**. You find it in Settings,
in the **Session** group ([Client settings](/docs/client-settings#behavior) covers what sits beside
it); the TV and controller layouts list it among the other general settings. It is a property of the
device and the network, so it is *not* part of a
[settings profile](/docs/profiles-and-links#what-a-profile-cant-change) — "Game" and "Work" cannot
disagree about it.

With auto-wake on, opening a saved host that is not advertising:

1. Fires one magic packet immediately, then **dials anyway**. Missing from mDNS does not mean
   unreachable — a host reached over a VPN or another subnet never advertises at all.
2. If the dial fails, shows a **"Waking…"** screen while it re-sends the packet every **6 seconds**
   and watches for the host once a second.
3. Gives up after **90 seconds**. The Apple and Android apps, and Punktfunk Console (the
   controller-driven shell), park there with **Try Again** and a cancel, rather than throwing an
   error — a cold box that needs another ten seconds is common. The Linux and Windows apps close
   the wait and tell you the host didn't come online; start the connect again to retry.
4. Reconnects when the host answers. In the Linux, Windows and Android apps, a host that came back
   on a different DHCP address has its saved record re-pointed at the new one.

Turn auto-wake **off** and a connect goes straight through with no packet and no wait. That is the
setting for hosts behind a VPN, which look offline when they are not.

There is also an explicit wake action, and it works whether or not auto-wake is on. It sits on a
saved host's own menu, and only appears when that host is offline *and* an address is known:

| Client | Explicit wake | Type a MAC in by hand |
|---|---|---|
| Linux (GTK) | **Wake host** — sends the packet and stops there | not offered |
| Windows | **Wake host** — sends the packet and stops there | **MAC (Wake-on-LAN)** under **Edit…** |
| macOS · iOS · iPadOS · tvOS | **Wake Host** — waits, showing the "Waking…" screen | **MAC address** in the **Edit Host** sheet |
| Android · Android TV | **Wake host** — waits, showing the "Waking…" screen | **Wake-on-LAN MAC** in **Edit host** |
| Punktfunk Console (controller shell) | on an offline host with a known address, the confirm button reads **Wake & Connect** — it waits, then connects | not offered |

Punktfunk Console carries the row too — **Wake hosts automatically**, in the same settings list the
desktop apps write — but its **Wake & Connect** button is an explicit action and appears whatever
that row says. In the Apple apps the same button appears when you drive them with a controller, but
there it does follow the auto-wake setting.

The Apple apps also publish a **Wake Host** action to Shortcuts, so an automation can wake a host
without opening the app. On iPhone and iPad it has a ready-made phrase: *"Wake ⟨host⟩ with
Punktfunk"*. It fails with a message if that host has no saved address yet.

On Android 17 and later the app needs the local-network permission before it can touch anything on
your LAN — discovery, the stream itself and a wake packet alike. It asks for it when you open the
host list, and shows an explanation with a link to system settings if you decline.

### On the Steam Deck

The [Decky plugin](/docs/steam-deck) has no wake button and no wake setting of its own. It starts
every stream through the client, so the wake is the client's, on exactly the terms above: a packet
the moment the host doesn't answer, re-sent every 6 seconds while the client watches for it once a
second, and the dial only when it really is back. It follows **Wake hosts automatically** in the
client's own settings — **Open Punktfunk → Settings** from the same panel — and is a no-op until the
client has learned that host's MAC address. (The plugin used to fire a packet itself and stretch the
connect budget to 75 seconds to cover the resume; a wait that watches for the host beats a fixed
budget, so that is gone.)

### From the command line

`punktfunk`, the client-side command, has a wake verb. It ships with the Linux `punktfunk-client`
packages and the Windows client; inside the Flatpak it is
`flatpak run --command=punktfunk io.unom.Punktfunk`. See [Host CLI](/docs/host-cli) for the rest of
it.

```bash
punktfunk wake <host-ref> [--wait]
```

A `<host-ref>` is a saved host's id, its name, or its address. Without `--wait` it sends the packet
and returns. With `--wait` it re-sends every 6 seconds and probes the host every second for up to 90
seconds, returning the moment the host answers.

| Exit code | Meaning |
|---|---|
| `0` | The packet was sent; with `--wait`, the host came back |
| `2` | With `--wait`, the host did not answer within 90 seconds |
| `5` | No saved host matches that reference, the name is ambiguous, or no address has been learned for it yet |
| `6` | That address is not a saved host — pair it first |

`punktfunk launch` wakes on its own: if auto-wake is on and it knows an address, it probes the host
first and runs the same wake-and-wait when it doesn't answer. You rarely need to chain the two.

## Arming the machine

Two things have to be true on the host machine, and Punktfunk changes neither of them — deciding
whether a machine may be woken off the network is yours to make.

1. **BIOS/UEFI.** Turn on the setting called **Wake on LAN**, **Wake on PCIe** or similar. Its name
   and location vary by vendor.
2. **The network card.** It has to be armed to wake on a magic packet.

### Check the host log first

This is the fastest diagnosis. On **Linux**, the host inspects the card carrying the address it
advertises, each time it starts advertising, and writes one of two lines:

```text
Wake-on-LAN armed (magic packet) on host NIC
```

```text
Wake-on-LAN is NOT armed on this host's NIC — clients cannot wake it from sleep.
```

The warning line goes on to name the interface and the exact command to fix it. The host only
reports; it never changes the card's settings. It stays silent when it cannot tell — `ethtool`
missing, or not enough privilege — rather than guessing, and it says nothing at all when mDNS
adverts are switched off (`PUNKTFUNK_MDNS=0` or `--no-mdns`), because then no address is published
either.

Read the line on the web console's **Logs** page, or in the journal with
`journalctl --user -u punktfunk-host`. See [Troubleshooting](/docs/troubleshooting#still-stuck).

**Windows and macOS hosts do not run this check**, so there is no log line to look for there.

### Linux

Ask the card what it is doing. `Supports Wake-on:` is the capability; `Wake-on:` is the current
setting. `g` means magic packet, `d` means disabled.

```bash
ethtool enp5s0
```

Arm it:

```bash
sudo ethtool -s enp5s0 wol g
```

On many systems that does not survive a reboot. Re-run `ethtool enp5s0` after the next boot to check,
and make it permanent through your distribution's network configuration if it reset.

### Windows

Open **Device Manager**, find the network adapter under **Network adapters**, and open its
properties. On the **Power Management** tab, allow the device to wake the computer; on the
**Advanced** tab, enable the adapter's magic-packet wake property if it has one. Exact wording
depends on the driver.

## Limits

- **Wired Ethernet is what works.** Waking over Wi-Fi is unreliable and depends entirely on the
  adapter and the platform.
- **Connect once while the host is awake**, on the same local network, before you rely on waking it.
  A host you only ever added by address, on a network where mDNS never reached it, has no learned
  address — the CLI will tell you so, and the apps will not offer the wake action. Typing the MAC in
  by hand is the way round that everywhere except the Linux app.
- **Magic packets are broadcasts.** They do not cross subnets, and they do not travel over a VPN or a
  mesh network. Client and host have to share a LAN segment for this to work at all.
- **Punktfunk never puts a host to sleep, and never wakes one on a schedule.** A packet goes out
  because a connect needs it, or because you asked for one.
- **There is no host-side switch.** The host publishes its MAC address and warns you when its card
  is not armed. Everything else — whether to wake, when, and how long to wait — is decided on the
  client.
