---
title: Wake-on-LAN
description: How Punktfunk clients wake a sleeping host — what has to have happened first, what a click does, the punktfunk wake command, and how to arm the machine so the packet actually lands.
---

A sleeping host answers nothing. Punktfunk remembers the host's network card address — its **MAC
address** — while the host is awake, and sends it a **magic packet** (the standard Wake-on-LAN
datagram) when you later ask to connect.

The Linux, Windows, Apple and Android apps do this by default; nothing to enable in Punktfunk. The
work is on the *machine*: its BIOS/UEFI and network card have to be armed to wake, and that is where
Wake-on-LAN usually fails — jump to [Arming the machine](#arming-the-machine) if that is what you
are here for.

## How it works

While running, the host advertises itself over mDNS, including `mac` — the card carrying the IP
clients reach it on first, then any other non-loopback cards as fallbacks, at most four. Each app
stores those addresses on its **saved host** record: Linux, Windows and Android refresh them
whenever they see the host advertise, the Apple app when you save the host and on every connect. A
sleeping host stops advertising, but the client still has the addresses on disk. That ordering is
the whole prerequisite:

> **The client must have seen the host awake at least once**, on a network where the host's mDNS
> advert reached it. Until then no address is known and there is nothing to wake with — the client
> says so rather than pretending. On every client but the Linux one you can also type the MAC in by
> hand; see the table below.

The packet goes **out of every one of the client's network interfaces** — from a socket bound to
that interface's own address, aimed at both its subnet broadcast address and `255.255.255.255` — on
UDP ports 9 and 7, repeated three times, plus a unicast to the host's last known address. (A
sleeping machine has no ARP entry, and an unbound broadcast leaves by the default route only — on a
VPN or mesh machine, not the LAN the host sleeps on.)

Neither the advert nor a magic packet is authenticated. A wrong address only makes the wake fail;
the host's certificate fingerprint still gates the connection. See [Security](/docs/security).

### Over Wi-Fi

A host on Wi-Fi wakes from the same packet via **WoWLAN** (Wake on Wireless LAN): the adapter stays
associated to your access point while the machine sleeps, the access point holds broadcast frames
for sleeping stations and releases them on the next beacon, and the adapter wakes the machine when
one is a magic packet. Punktfunk publishes a Wi-Fi card's address exactly like a wired one, so the
client side is unchanged — but the card has to be armed for it, a different switch from the wired
one. See [Linux (Wi-Fi)](#linux-wi-fi) and [Windows](#windows) below.

Two things can still stop it, neither visible from Punktfunk:

- Some access points and mesh systems drop or rate-limit broadcast traffic to sleeping stations
  (often as "multicast enhancement", "broadcast filtering" or IGMP snooping). If wired hosts wake
  and a Wi-Fi one never does, turn that off first.
- Some laptops and adapters cut power to the Wi-Fi card in deeper sleep states, dropping the
  association and with it any chance of a wake.

## Waking from a client

**Auto-wake on connect** is a client setting, **on by default**, in Settings under **Session**
([Client settings](/docs/client-settings#behavior)); the TV and controller layouts list it among
the general settings. A property of the device and the network, so *not* part of a
[settings profile](/docs/profiles-and-links#what-a-profile-cant-change).

With auto-wake on, opening a saved host that is not advertising:

1. Fires one magic packet immediately, then **dials anyway** — missing from mDNS does not mean
   unreachable; a host reached over a VPN or another subnet never advertises at all.
2. If the dial fails, shows a **"Waking…"** screen while it re-sends the packet every **6 seconds**
   and watches for the host once a second.
3. Gives up after **90 seconds**. The Apple and Android apps, and Punktfunk Console (the
   controller-driven shell), park there with **Try Again** and a cancel rather than an error — a
   cold box that needs another ten seconds is common. The Linux and Windows apps close the wait and
   tell you the host didn't come online; start the connect again to retry.
4. Reconnects when the host answers. In the Linux, Windows and Android apps, a host that came back
   on a different DHCP address has its saved record re-pointed at the new one.

With auto-wake **off**, a connect goes straight through with no packet and no wait — the setting for
hosts behind a VPN, which look offline when they are not.

There is also an explicit wake action, independent of auto-wake, on a saved host's own menu. It
appears only when that host is offline *and* an address is known:

| Client | Explicit wake | Type a MAC in by hand |
|---|---|---|
| Linux (GTK) | **Wake host** — sends the packet and stops there | not offered |
| Windows | **Wake host** — sends the packet and stops there | **MAC (Wake-on-LAN)** under **Edit…** |
| macOS · iOS · iPadOS · tvOS | **Wake Host** — waits, showing the "Waking…" screen | **MAC address** in the **Edit Host** sheet |
| Android · Android TV | **Wake host** — waits, showing the "Waking…" screen | **Wake-on-LAN MAC** in **Edit host** |
| Punktfunk Console (controller shell) | on an offline host with a known address, the confirm button reads **Wake & Connect** — it waits, then connects | not offered |

Punktfunk Console carries the row too — **Wake hosts automatically**, in the same settings list the
desktop apps write — but its **Wake & Connect** button is explicit and appears whatever that row
says. In the Apple apps the same button appears when you drive them with a controller, but there it
follows the auto-wake setting.

The Apple apps also publish a **Wake Host** action to Shortcuts, so an automation can wake a host
without opening the app. On iPhone and iPad it has a ready-made phrase: *"Wake ⟨host⟩ with
Punktfunk"*. It fails with a message if that host has no saved address yet.

On Android 17 and later the app needs the local-network permission before it can touch anything on
your LAN — discovery, the stream and a wake packet alike. It asks when you open the host list, and
shows an explanation with a link to system settings if you decline.

### On the Steam Deck

The [Decky plugin](/docs/steam-deck) has no wake button or wake setting of its own. It starts every
stream through the client, so the wake is the client's, on exactly the terms above. It follows
**Wake hosts automatically** in the client's own settings (**Open Punktfunk → Settings** from the
same panel) and is a no-op until the client has learned that host's MAC address.

### From the command line

`punktfunk`, the client-side command, has a wake verb. It ships with the Linux `punktfunk-client`
packages and the Windows client; inside the Flatpak it is
`flatpak run --command=punktfunk io.unom.Punktfunk`. See [Host CLI](/docs/host-cli) for the rest.

```bash
punktfunk wake <host-ref> [--wait]
```

`<host-ref>` is a saved host's id, name, or address. Without `--wait` it sends the packet and
returns. With `--wait` it re-sends every 6 seconds and probes the host every second for up to 90
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

Two things have to be true on the host machine, and Punktfunk changes neither — whether a machine
may be woken off the network is your decision.

1. **BIOS/UEFI.** Turn on the setting called **Wake on LAN**, **Wake on PCIe** or similar. Its name
   and location vary by vendor.
2. **The network card.** It has to be armed to wake on a magic packet.

### Check the host log first

The fastest diagnosis. On **Linux**, each time the host starts advertising it inspects the card
carrying the advertised address and writes one line. A wired card:

```text
Wake-on-LAN armed (magic packet) on host NIC
```

```text
Wake-on-LAN is NOT armed on this host's NIC — clients cannot wake it from sleep.
```

A Wi-Fi card, armed through a different mechanism and asked about separately
(`iw phy … wowlan show`, not `ethtool`):

```text
Wake-on-WLAN armed (magic packet) on host Wi-Fi NIC
```

```text
Wake-on-WLAN is NOT armed on this host's Wi-Fi NIC — clients cannot wake it from sleep.
```

The warning line names the interface and the exact command to fix it. The host only reports; it
never changes the card's settings, stays silent when it cannot tell (`iw`/`ethtool` missing, a
driver that doesn't answer, not enough privilege), and says nothing when mDNS adverts are off
(`PUNKTFUNK_MDNS=0` or `--no-mdns`) — then no address is published either.

Read the line on the web console's **Logs** page, or with
`journalctl --user -u punktfunk-host`. See [Troubleshooting](/docs/troubleshooting#still-stuck).

**Windows and macOS hosts do not run this check**, so there is no log line to look for there.

### Linux (wired)

Ask the card what it is doing. `Supports Wake-on:` is the capability; `Wake-on:` is the current
setting. `g` means magic packet, `d` means disabled.

```bash
ethtool enp5s0
```

Arm it:

```bash
sudo ethtool -s enp5s0 wol g
```

On many systems that does not survive a reboot. Re-run `ethtool enp5s0` after the next boot, and
make it permanent through your distribution's network configuration if it reset.

### Linux (Wi-Fi)

`ethtool` is the wrong tool here — most wireless drivers report `Wake-on: d` whether or not they are
armed, because the trigger lives in the wireless stack. Ask `iw`, using the *phy* behind the
interface (`/sys/class/net/wlan0/phy80211/name`, usually `phy0`):

```bash
iw phy phy0 wowlan show
```

`WoWLAN is disabled` means no wake. Armed looks like this; the `* wake up on magic packet` line is
the one that matters:

```text
WoWLAN is enabled:
 * wake up on magic packet
```

Arm it:

```bash
sudo iw phy phy0 wowlan enable magic-packet
```

That setting is per-phy and NetworkManager re-applies its own on every connection, so on a
NetworkManager system set it on the connection instead — this survives reboots and reconnects:

```bash
sudo nmcli connection modify <connection> 802-11-wireless.wake-on-wlan magic
```

`iw phy phy0 wowlan show` reporting `command failed: Operation not supported` means the driver has no
WoWLAN support at all; that adapter cannot be woken over Wi-Fi. Check `iw list | grep -A5 "WoWLAN"`
for what the hardware claims to support.

### Windows

Open **Device Manager**, find the adapter under **Network adapters**, and open its properties. On
the **Power Management** tab, allow the device to wake the computer; on the **Advanced** tab, enable
the adapter's magic-packet wake property if it has one. Exact wording depends on the driver.

Wi-Fi adapters use the same two tabs. The **Advanced** property is often called **Wake on Magic
Packet** there too, sometimes **Wake on Wireless LAN**; many Wi-Fi drivers expose neither, and those
cannot be woken over Wi-Fi. `powercfg /devicequery wake_armed` lists every device currently allowed
to wake the machine — if the adapter is not in it, nothing on the network can wake this host.

## Limits

- **Wired Ethernet is the sure thing; Wi-Fi works when the adapter supports WoWLAN.** Punktfunk
  sends the same packet either way, but whether a sleeping adapter is still listening is the
  adapter's and the access point's decision — see [Over Wi-Fi](#over-wi-fi).
- **Connect once while the host is awake**, on the same local network, before you rely on waking it.
  A host only ever added by address, where mDNS never reached the client, has no learned address —
  the CLI says so, and the apps don't offer the wake action. Typing the MAC in by hand is the way
  round that everywhere except the Linux app.
- **Magic packets are broadcasts.** They do not cross subnets, a VPN or a mesh network. Client and
  host have to share a LAN segment.
- **Punktfunk never puts a host to sleep, and never wakes one on a schedule.** A packet goes out
  because a connect needs it, or because you asked for one.
- **There is no host-side switch.** The host publishes its MAC address and warns when its card is
  not armed. Whether to wake, when, and how long to wait is decided on the client.
