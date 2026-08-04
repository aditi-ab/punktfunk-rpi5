# tvOS — App Store metadata

Client only, living-room framing. Things the other platforms have that the **Apple TV does not**,
and which the copy therefore avoids claiming:

- **No microphone uplink.** There is no usable audio input on tvOS, so the "your Mac becomes the
  headset" line does not transfer.
- **No gamepad console shell.** `ShotScenes` builds the gamepad home/settings screens for iOS and
  macOS only — tvOS uses the native focus engine instead.
- **No AV1.** Apple TV 4K has no AV1 hardware decoder; HEVC and H.264 only.
- Mouse/keyboard capture exists on tvOS but is not a living-room story, so it stays out.

Kept, and genuinely tvOS-shaped: Siri Remote pointer navigation (`SiriRemotePointer`), controllers
including the full DualSense feedback set, HDR passthrough, and Wake-on-LAN — which is the single
best Apple TV feature, because it is what removes the trip to the other room.

- **Name:** Punktfunk
- **Subtitle (DE):** Schnell, lokal & offen.
- **Subtitle (EN):** Fast, local & open.

---

## Promotional Text (DE) — max 170 characters

### Primary (161)

```
Anschalten, Host wählen, spielen: Punktfunk weckt deinen Gaming-PC per Wake-on-LAN und verbindet sich, sobald er wach ist. In 4K, mit HDR, mit deinem Controller.
```

### Alternate A — leads on the picture (157)

```
Dein Gaming-PC am großen Bildschirm – in genau der Auflösung und Bildrate deines Fernsehers, mit HDR. Ohne Konto, ohne Cloud, nur über dein eigenes Netzwerk.
```

### Alternate B — leads on the DualSense (160)

```
Dein DualSense am Apple TV, vollständig: Rumble, adaptive Trigger, Lightbar, Touchpad und Gyro gehen bis ins Spiel durch. Dazu Profile pro Host und Wake-on-LAN.
```

## Promotional Text (EN) — max 170 characters

### Primary (160)

```
Turn on, pick a host, play: Punktfunk wakes your gaming PC over Wake-on-LAN and connects as soon as it's up. In 4K, with HDR, with the controller in your hands.
```

### Alternate A — leads on the picture (148)

```
Your gaming PC on the big screen — at your TV's exact resolution and refresh rate, with HDR. No account, no cloud, nothing leaving your own network.
```

---

## Description (DE) — max 4000 characters

```
Punktfunk macht aus deinem Apple TV die Konsole für den Gaming-PC, der ohnehin schon im Haus steht – in 4K, mit HDR, über dein eigenes Netzwerk, ohne Konto und ohne Cloud.

Punktfunk besteht aus zwei Hälften: einem Host auf dem PC, von dem du streamst, und dieser App auf dem Gerät, auf dem du spielst. Der Host ist quelloffen und kostenlos, läuft auf Linux und auf Windows 11 – auch headless auf einem Rechner, an dem gar kein Monitor hängt.

VOM SOFA AUS, VON ANFANG BIS ENDE

Anschalten, Host auswählen, spielen. Die App findet Hosts im Netzwerk von allein. Beim ersten Mal koppelst du einmalig mit einer PIN, danach verbindet sich der Apple TV über eine gepinnte Identität – kein Konto, kein Login, kein Abtippen von IP-Adressen. Steht dein Gaming-PC im Standby, weckt ihn Punktfunk per Wake-on-LAN und verbindet sich, sobald er wach ist. Niemand muss dafür aufstehen.

DAS BILD, DAS DEIN FERNSEHER WIRKLICH KANN

Für den Apple TV legt der Host ein echtes virtuelles Display an – in genau der Auflösung und Bildrate, die dein Fernseher meldet, bis 4K. Kein Skalieren, keine schwarzen Balken, und die Monitore am PC werden nicht umsortiert. Dekodiert wird in Hardware über VideoToolbox (HEVC und H.264), HDR wird als PQ durchgereicht, statt es flach zu rechnen.

CONTROLLER, VOLLSTÄNDIG

DualSense, Xbox- und weitere MFi-kompatible Controller. Beim DualSense gehen Rumble, Lightbar, Player-LEDs, adaptive Trigger, Touchpad und Gyro bis ins Spiel durch. Welchen Typ das virtuelle Gamepad am Host annimmt, richtet sich nach dem, was bei dir wirklich in der Hand liegt. Bedienen lässt sich alles mit der Siri Remote oder komplett mit dem Controller – die Oberfläche ist für die Fernbedienung gebaut, nicht für eine Maus.

DEINE BIBLIOTHEK AUF DEM FERNSEHER

Installierte Steam-Titel und selbst hinzugefügte Spiele erscheinen als Raster mit Artwork und starten direkt vom Sofa aus. Mehrere Geräte können gleichzeitig streamen, jedes auf seinem eigenen Display – der Apple TV im Wohnzimmer stört also niemanden, der am Schreibtisch weiterarbeitet.

SCHNELL, WEIL UNS DER GANZE WEG GEHÖRT

Die nativen Apps sprechen punktfunk/1: eine QUIC-Steuerebene und eine verschlüsselte Datenebene mit Vorwärtsfehlerkorrektur. Ein gestuftes Overlay zeigt Bildrate, Bitrate und Latenz – über zwei Maschinen hinweg um den Uhrenversatz korrigiert, also eine Messung und kein Versprechen. Ein Geschwindigkeitstest pro Host schlägt eine passende Bitrate für dein Netzwerk vor.

WAS DU BRAUCHST

Einen Punktfunk-Host auf einem Linux-PC oder auf Windows 11 (22H2 oder neuer) im selben Netzwerk. Für die beste Erfahrung hängt der Apple TV am Kabel oder an einem guten 5-GHz-WLAN. Der Host ist quelloffen (MIT/Apache-2.0) und kostenlos – Anleitungen und Quellcode findest du auf punktfunk.unom.io.

Kein Konto. Keine Cloud. Keine Telemetrie. Die App erfasst keine Daten über dich.
```

---

## Description (EN) — max 4000 characters

```
Punktfunk turns your Apple TV into a console for the gaming PC you already own — in 4K, with HDR, over your own network, with no account and no cloud.

Punktfunk comes in two halves: a host on the PC you stream from, and this app on the device you play on. The host is open source and free, and runs on Linux and on Windows 11 — including headless, on a machine with no monitor attached at all.

FROM THE COUCH, START TO FINISH

Turn on, pick a host, play. The app finds hosts on your network by itself. The first time, you pair once with a PIN; after that your Apple TV reconnects on a pinned identity — no account, no login, no typing IP addresses with a remote. If your gaming PC is asleep, Punktfunk wakes it over Wake-on-LAN and connects as soon as it is up. Nobody has to get up to make that happen.

THE PICTURE YOUR TV CAN ACTUALLY SHOW

For your Apple TV, the host creates a real virtual display at exactly the resolution and refresh rate your TV reports, up to 4K. No scaling, no black bars, and the monitors on your PC are left where they are. Decoding is done in hardware through VideoToolbox (HEVC and H.264), and HDR is passed through as PQ rather than flattened.

CONTROLLERS, IN FULL

DualSense, Xbox, and other MFi-compatible controllers. On a DualSense, rumble, lightbar, player LEDs, adaptive triggers, touchpad, and gyro all reach the game. The virtual gamepad the host presents takes its type from the controller actually in your hands. Everything is navigable with the Siri Remote or entirely with a controller — the interface is built for a remote, not for a mouse.

YOUR LIBRARY ON THE BIG SCREEN

Installed Steam titles and games you add yourself appear as a grid with artwork, ready to launch from the couch. Several devices can stream at once, each on its own display — so the Apple TV in the living room does not disturb anyone still working at the desk.

FAST, BECAUSE WE OWN THE WHOLE PATH

The native apps speak punktfunk/1: a QUIC control plane and an encrypted data plane with forward error correction. A tiered overlay shows frame rate, bitrate, and latency — corrected for clock skew across the two machines, so it is a measurement rather than a claim. A per-host speed test suggests a bitrate that matches your network.

WHAT YOU NEED

A Punktfunk host on a Linux PC or on Windows 11 (22H2 or later) on the same network. For the best experience, put your Apple TV on Ethernet or on good 5 GHz Wi-Fi. The host is open source (MIT/Apache-2.0) and free — guides and source at punktfunk.unom.io.

No account. No cloud. No telemetry. This app collects no data about you.
```

---

## Keywords — max 100 characters

### DE (93)

```
streaming,spiele,gaming,controller,gamepad,wohnzimmer,fernseher,pc,linux,windows,4k,hdr,couch
```

### EN (91)

```
streaming,gaming,controller,gamepad,livingroom,tv,pc,linux,windows,4k,hdr,couch,remote,play
```

Same exclusions as macOS: no `Moonlight`, `GameStream`, `NVIDIA`, or `Steam` in the keyword field.
