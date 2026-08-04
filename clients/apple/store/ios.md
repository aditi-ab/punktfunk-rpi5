# iOS / iPadOS — App Store metadata

Existing, unchanged:

- **Name:** Punktfunk
- **Subtitle (DE):** Schnell, lokal & offen.

Only the Promotional Text is new here. It is the one field that can be changed **without** a new
build or a review, so it is the right place for "what landed most recently".

---

## Promotional Text (DE) — max 170 characters

### Primary (160)

```
Neu: Profile pro Host – Auflösung, Bitrate und Ton einmal einstellen, dann mit einem Tipp verbinden. Dazu Live Activity, Sperrbildschirm-Widget und Wake-on-LAN.
```

### Alternate A — evergreen hook, no "new" claim (156)

```
Dein Gaming-PC auf dem iPhone, in dessen exakter Auflösung – ohne Konto, ohne Cloud, nur dein Netzwerk. Hardware-Decoding, HDR und dein DualSense mit allem.
```

### Alternate B — leads on the DualSense (161)

```
Dein DualSense, vollständig: Rumble, adaptive Trigger, Lightbar, Touchpad und Gyro gehen bis ins Spiel durch. Dazu Profile pro Host und Wake-on-LAN vom Sofa aus.
```

### Alternate C — leads on latency (153)

```
Kein Konto, keine Cloud, kein Umweg: punktfunk/1 fährt über QUIC direkt zu deinem PC. Auflösungswechsel mitten im Stream, ohne die Verbindung zu trennen.
```

---

## Promotional Text (EN) — max 170 characters

### Primary (152)

```
New: per-host profiles — set resolution, bitrate and audio once, then connect with one tap. Plus Live Activities, a Lock Screen widget, and Wake-on-LAN.
```

### Alternate A — evergreen hook (159)

```
Your gaming PC on your iPhone, at your iPhone's exact resolution — no account, no cloud, just your network. Hardware decoding, HDR, and your DualSense in full.
```

### Alternate B — leads on the DualSense (160)

```
Your DualSense, in full: rumble, adaptive triggers, lightbar, touchpad and gyro all reach the game. Plus per-host profiles and Wake-on-LAN from across the room.
```

---

## Notes on the claims

- "Profile pro Host" shipped in **v0.22.0** (`25b12780`, `80c0ca69`) and is in every tag since. It is
  the strongest recent user-facing Apple feature, so "Neu" is defensible for one release cycle — but
  drop the word once 0.25 ships something newer.
- Live Activities and the Hosts widget shipped long ago (`ba1caf02`, in v0.15.0+). They are safe to
  *mention* but should not be called "neu".
- The only Apple-visible feature unique to **v0.24.0** is the "Forward controllers" off switch
  (`b297542c`), which is too niche to headline.
