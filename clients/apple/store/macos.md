# macOS — App Store metadata

> **Scope correction.** The Mac app is a **client only**. There is no macOS host: `punktfunk-host`
> has no macOS capture, virtual-display, or encode backend (the two `cfg!(target_os = "macos")` hits
> in the host crate are OS *detection* for the host tile and a path helper; the loopback-test host
> is a synthetic frame source for `test-loopback.sh`, not a shippable host). A macOS host is a
> feasibility study — it needs four new backends and the private `CGVirtualDisplay` API.
> None of the copy below claims a Mac can host, and it should not until that ships.

- **Name:** Punktfunk
- **Subtitle (DE):** Schnell, lokal & offen.
- **Subtitle (EN):** Fast, local & open.

---

## Promotional Text (DE) — max 170 characters

### Primary (164)

```
Neu: Profile pro Host – ein Mac, mehrere Gaming-PCs, jeder mit eigenen Einstellungen. Dazu AV1-Hardware-Decoding auf M3 und neuer, HDR und volles 4:4:4 für Schrift.
```

### Alternate (156)

```
Dein Gaming-PC im Fenster oder im Vollbild, in der exakten Auflösung deines Displays. Maus und Tastatur gehen durch, Auflösungswechsel ohne neue Verbindung.
```

## Promotional Text (EN) — max 170 characters

### Primary (161)

```
New: per-host profiles — one Mac, several gaming PCs, each with its own settings. Plus AV1 hardware decoding on M3 and later, HDR, and full 4:4:4 for crisp text.
```

### Alternate (156)

```
Your gaming PC in a window or full screen, at your display's exact resolution. Mouse and keyboard pass straight through; resize without dropping the stream.
```

---

## Description (DE) — max 4000 characters

```
Punktfunk streamt deinen Gaming-PC auf den Mac – in der exakten Auflösung und Bildwiederholrate deines Displays, über dein eigenes Netzwerk, ohne Konto und ohne Cloud.

Punktfunk besteht aus zwei Hälften: einem Host auf dem PC, von dem du streamst, und dieser App auf dem Gerät, auf dem du spielst. Der Host ist quelloffen und kostenlos, läuft auf Linux und auf Windows 11 – auf dem Gaming-Rig unterm Schreibtisch, auf einem Laptop oder headless auf einem Server, an dem gar kein Monitor hängt.

DEIN MAC BEKOMMT SEIN EIGENES DISPLAY

Für jede Verbindung legt der Host ein echtes virtuelles Display an – in genau der Auflösung und Bildrate, die dein Mac meldet. Kein Skalieren, keine schwarzen Balken, kein Umsortieren deiner echten Monitore. Änderst du mitten im Stream die Fenstergröße oder gehst auf Vollbild, wird die Auflösung neu ausgehandelt, ohne die Verbindung zu trennen. Mehrere Geräte können gleichzeitig streamen, jedes auf seinem eigenen Display.

SCHNELL, WEIL UNS DER GANZE WEG GEHÖRT

Die nativen Apps sprechen punktfunk/1: eine QUIC-Steuerebene und eine verschlüsselte Datenebene mit Vorwärtsfehlerkorrektur, die Auflösung und Bildrate mitten im Stream wechselt, ohne neu zu verbinden. Dekodiert wird in Hardware über VideoToolbox – H.264, HEVC und AV1 auf Macs, die AV1 in Hardware können (M3 und neuer).

FÜR DEN MAC GEMACHT

• Im Fenster oder im Vollbild, auf jedem angeschlossenen Display
• Maus und Tastatur gehen vollständig durch – Klick zum Fangen, Cmd+Esc oder Ctrl+Alt+Shift+Q zum Freigeben
• Ein Stream-Menü in der Menüleiste: Maus freigeben, Trennen, Statistik einblenden
• Mikrofon-Uplink mit Echounterdrückung – dein Mac wird zum Headset am PC
• HDR mit PQ-Passthrough und ein optionaler Vollchroma-Modus (4:4:4), damit kleine Schrift und feine Linien scharf bleiben

CONTROLLER, VOLLSTÄNDIG

DualSense, Xbox- und weitere MFi-kompatible Controller. Beim DualSense gehen Rumble, Lightbar, Player-LEDs, adaptive Trigger, Touchpad und Gyro bis ins Spiel durch. Welchen Typ das virtuelle Gamepad am Host annimmt, richtet sich nach dem, was bei dir wirklich in der Hand liegt.

DEINE BIBLIOTHEK, DEIN NETZWERK

Installierte Steam-Titel und selbst hinzugefügte Spiele erscheinen als Raster mit Artwork und starten direkt. Hosts findet die App im Netzwerk von allein. Beim ersten Mal koppelst du einmalig mit einer PIN, danach verbindet sich der Mac über eine gepinnte Identität aus deinem Schlüsselbund – kein Konto, kein Login. Einen schlafenden PC weckt Punktfunk per Wake-on-LAN.

MESSEN STATT GLAUBEN

Ein gestuftes Overlay zeigt Bildrate, Bitrate und Latenz – über zwei Maschinen hinweg um den Uhrenversatz korrigiert, also eine Messung und kein Versprechen. Ein Geschwindigkeitstest pro Host schlägt eine passende Bitrate vor. Profile halten pro Host fest, wie gestreamt werden soll.

WAS DU BRAUCHST

Einen Punktfunk-Host auf einem Linux-PC oder auf Windows 11 (22H2 oder neuer) im selben Netzwerk. Der Host ist quelloffen (MIT/Apache-2.0) und kostenlos – Anleitungen und Quellcode findest du auf punktfunk.unom.io. Diese App ist der Client: ein Mac kann derzeit nicht selbst Host sein.

Kein Konto. Keine Cloud. Keine Telemetrie. Die App erfasst keine Daten über dich.
```

---

## Description (EN) — max 4000 characters

```
Punktfunk streams your gaming PC to your Mac — at your display's exact resolution and refresh rate, over your own network, with no account and no cloud.

Punktfunk comes in two halves: a host on the PC you stream from, and this app on the device you play on. The host is open source and free, and runs on Linux and on Windows 11 — on the gaming rig under your desk, on a laptop, or headless on a server with no monitor attached at all.

YOUR MAC GETS A DISPLAY OF ITS OWN

For every connection, the host creates a real virtual display at exactly the resolution and refresh rate your Mac reports. No scaling, no black bars, no rearranging your actual monitors. Resize the window mid-stream or go full screen and the resolution is renegotiated without dropping the connection. Several devices can stream at once, each on its own display.

FAST, BECAUSE WE OWN THE WHOLE PATH

The native apps speak punktfunk/1: a QUIC control plane and an encrypted data plane with forward error correction, able to change resolution and frame rate mid-stream without reconnecting. Decoding is done in hardware through VideoToolbox — H.264, HEVC, and AV1 on Macs with an AV1 hardware decoder (M3 and later).

BUILT FOR THE MAC

• In a window or full screen, on any attached display
• Mouse and keyboard pass straight through — click to capture, Cmd+Esc or Ctrl+Alt+Shift+Q to release
• A Stream menu in the menu bar: release the mouse, disconnect, toggle the stats overlay
• Microphone uplink with echo cancellation — your Mac becomes the headset on your PC
• HDR with PQ passthrough, plus an optional full-chroma (4:4:4) mode that keeps small text and fine UI lines sharp

CONTROLLERS, IN FULL

DualSense, Xbox, and other MFi-compatible controllers. On a DualSense, rumble, lightbar, player LEDs, adaptive triggers, touchpad, and gyro all reach the game. The virtual gamepad the host presents takes its type from the controller actually in your hands.

YOUR LIBRARY, YOUR NETWORK

Installed Steam titles and games you add yourself appear as a grid with artwork, ready to launch. The app finds hosts on your network by itself. The first time, you pair once with a PIN; after that your Mac reconnects on a pinned identity stored in your keychain — no account, no login. Punktfunk can wake a sleeping PC over Wake-on-LAN.

MEASURED, NOT PROMISED

A tiered overlay shows frame rate, bitrate, and latency — corrected for clock skew across the two machines, so it is a measurement rather than a claim. A per-host speed test suggests a bitrate that matches your link. Profiles remember how each host should be streamed.

WHAT YOU NEED

A Punktfunk host on a Linux PC or on Windows 11 (22H2 or later) on the same network. The host is open source (MIT/Apache-2.0) and free — guides and source at punktfunk.unom.io. This app is the client: a Mac cannot currently act as a host.

No account. No cloud. No telemetry. This app collects no data about you.
```

---

## Keywords — max 100 characters

Comma-separated, **no spaces after the commas** (spaces count against the limit). The app name and
the subtitle are already indexed, so `punktfunk`, `schnell`, `lokal`, and `offen` are deliberately
absent — repeating them would waste characters.

### DE (97)

```
streaming,spiele,remote,desktop,fernzugriff,pc,linux,windows,controller,gamepad,latenz,quelloffen
```

### EN (95)

```
streaming,remote,desktop,pc,linux,windows,gaming,controller,gamepad,latency,selfhosted,lan,play
```

**Deliberately excluded:** `Moonlight`, `GameStream`, `NVIDIA`, `Steam`. Punktfunk genuinely is
GameStream-compatible and does read your Steam library, but App Store Review Guideline 4.1 and the
metadata rules disallow third-party app, product, and company names in the **keyword** field — it is
a routine rejection. Saying it in the description is fine; the current descriptions avoid naming
Moonlight and mention Steam only as a factual statement about your own library.

The previous keyword set (`Game-Streaming, Lokal, Open-Source, Gaming`) spent characters on spaces,
on `Lokal` (already in the subtitle), and on both `Game-Streaming` and `Gaming`, which share a stem.
