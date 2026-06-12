---
title: "M2 — Moonlight Host"
description: "Stream to a stock Moonlight client on a client-sized virtual display."
---


The shippable milestone (plan §8). A stock Moonlight/Artemis client discovers this host,
pairs, launches, and gets video (then input, then audio) on a client-sized virtual display.
Ground-truth protocol reference: [`research/gamestream-protocol-research.json`](research/gamestream-protocol-research.json)
(distilled from Sunshine + moonlight-common-c source; cite those for byte-level detail).

## Architecture (respects the "one core" invariant)

- **punktfunk-core** gains a **P1 GameStream wire codec** (`ProtocolPhase::P1GameStream`, the
  hook already exists): the exact RTP+`NV_VIDEO_PACKET` framing, the GameStream FEC shard
  layout, and the video/audio AES-GCM/CBC paths. Hot path, native threads, **no async**.
  Kept beside punktfunk's native internal format (P2), selected by phase.
- **punktfunk-host** gains the **control plane** (tokio/axum OK — I/O-bound, not the hot path):
  mDNS discovery, nvhttp serverinfo + the 4-phase pairing, the RTSP handshake, the ENet
  control stream + input injection, the virtual-display lifecycle, and Opus audio encode.

## Port map (base 47989; Moonlight derives all by offset)

| Port | Proto | Role |
|---|---|---|
| 47989 | TCP | HTTP nvhttp (unpaired: /serverinfo, /pair PIN flow) |
| 47984 | TCP | HTTPS nvhttp (paired; **client-cert pinned**) — /launch, /resume, … |
| 48010 | TCP | RTSP (OPTIONS/DESCRIBE/SETUP/ANNOUNCE/PLAY) |
| 47998 | UDP | Video RTP (+ RS-FEC, optional AES-GCM) |
| 47999 | UDP | Audio RTP (Opus, RS-FEC 4+2, optional **AES-CBC**) |
| 48000 | UDP | ENet control stream (AES-GCM) + remote input |
| 5353  | UDP | mDNS `_nvstream._tcp.local` advertisement |

## Key wire facts (the non-obvious ones)

- **Video datagram** = `RTP_PACKET(12, BIG-endian)` + `reserved[4]` + `NV_VIDEO_PACKET(16,
  LITTLE-endian)` + payload. Endianness differs *within the same packet*. `header=0x80|0x10`.
- `fecInfo` (u32 LE) = `(dataShards<<22)|(fecIndex<<12)|(fecPercentage<<4)`; parityShards is
  **recomputed** by the client as `ceil(dataShards*pct/100)` — must match exactly.
- `multiFecBlocks` = `(blockIdx<<4)|((nBlocks-1)<<6)`; **≤4 FEC blocks/frame**, ≤255 shards/block.
- Each frame's bitstream is prefixed with an 8-byte `video_short_frame_header_t`
  (`headerType=0x01`, `frameType` 2=IDR, `lastPayloadLen`) before striping into shards.
- Shard size = `packetSize + 16`. Data shards first, then parity, over a contiguous RTP
  sequence range. Last data shard zero-padded.
- **Video crypto** (when `SS_ENC_VIDEO` negotiated): AES-128-GCM, key = raw 16-byte RIKEY
  (from `/launch?rikey=`), IV = `counter_le[8]||0,0,0||'V'(0x56)`, **NO AAD**, 32-byte
  `ENC_VIDEO_HEADER{iv[12],frameNumber,tag[16]}` prefix; **FEC first, then encrypt per shard**.
- **Pairing**: PIN key = `SHA-256(salt[16] || ascii_pin)[..16]`; AES-128-**ECB** (no padding)
  for the challenge blocks; SHA-256 rolling hashes; RSA-SHA256 signatures over X.509 certs;
  the client cert is pinned for subsequent HTTPS. 4 phases over `/pair?phrase=…`.
- **RTSP** `Session: DEADBEEFCAFE;timeout = 90` (literal), `Transport: server_port=<p>`,
  `streamid=video/0/0` / `control/13/0`. ANNOUNCE carries the negotiated config
  (`x-nv-video[0].*`, `x-nv-vqos[0].*`) → maps to `punktfunk_core::Config`.

## The two highest interop risks (validate EARLY)

1. **RS-FEC matrix compatibility.** Sunshine + Moonlight both use **nanors** (GF(2⁸), poly
   0x11d, Vandermonde systematic). punktfunk-core uses `reed-solomon-erasure` (Cauchy) — parity
   bytes likely **don't match**, so Moonlight silently fails to recover any frame with a lost
   data shard. Mitigation: **on a clean LAN with no loss the client never runs RS decode**, so
   defer this — get a frame decoded first, then FFI/port nanors for loss recovery.
2. **Crypto layout.** punktfunk's `SessionCrypto` (salt + seq-as-AAD) is wire-incompatible. P1
   needs a separate GameStream GCM path. Mitigation: **video encryption is negotiated and
   usually off on LAN** — implement plaintext video first, add GCM later.

## Phasing (each phase independently testable with a real Moonlight client)

- **P1.1 — Discovery + serverinfo + pairing.** mDNS `_nvstream._tcp`, HTTP/HTTPS nvhttp,
  `/serverinfo` XML, the 4-phase pairing + cert pinning. *Acceptance: Moonlight discovers,
  pairs (PIN), and shows the host as ready.* ← first slice.
- **P1.2 — Launch + RTSP + virtual display.** `/launch` (parse rikey/rikeyid/mode), the RTSP
  handshake, negotiate `Config`, create a wlroots virtual output sized to the client.
  *Acceptance: Moonlight completes RTSP and the host stands up the UDP streams.*
- **P1.3 — Video (punktfunk-core P1 codec), plaintext, clean-LAN.** RTP+NV framing + FEC shard
  layout in punktfunk-core; wire M0's NVENC AUs → UDP 47998. *Acceptance: Moonlight DISPLAYS video.*
- **P1.4 — Control + input.** ENet (`rusty_enet`) control stream; decode input → `inject.rs`
  (uinput/reis); request-IDR → force NVENC keyframe. *Acceptance: mouse/keyboard work.*
- **P1.5 — Robustness: FEC recovery + encryption.** nanors-exact FEC; per-shard AES-GCM.
  *Acceptance: stable under `tc netem` loss; encrypted streams.*
- **P1.6 — Audio + polish.** Opus + audio RTP/FEC/CBC (UDP 47999); disconnect teardown; KWin
  backend for the user's KDE box. *Acceptance: full game stream with sound — the M2 goal.*

## Crates (verified available)

`mdns-sd` 0.20 (discovery) · `axum` 0.8 + `rustls` + `tokio-rustls` (nvhttp/HTTPS, custom
`ClientCertVerifier` for pinning) · `rcgen` 0.14 + `x509-parser` 0.18 + `rsa`/`sha2`/`aes`/
`ecb` (pairing crypto) · hand-rolled RTSP over `tokio::net::TcpListener` · `rusty_enet` 0.4
(control) · `opus` 0.3 (audio) · `reis` 0.6 + `input-linux` (input) · `aes-gcm` (already in
core) for the P1 video/control GCM path; nanors (FFI/port) for FEC recovery in P1.5.

## Testing note

The host is headless; end-to-end needs a **stock Moonlight client on the LAN** pointed at
this box (manual "add host" by IP works without mDNS). P1.1 is testable with `curl` against
`/serverinfo` + the Moonlight pair flow; P1.3+ needs a client that can display.
