---
title: "Roadmap"
description: "Decided next goals and the longer-term bets."
---


Decided 2026-06-10 (research-grounded; see commit history), extended since.

**Done & live (on `main`):** #1 KDE reliability (Phase 1+2), #2 client compositor options (full
stack incl. the macOS client), #4 mic passthrough, #5 touch (host path) + **rich UHID DualSense**
— input + adaptive-trigger/LED feedback over the new `0xCC`/`0xCD` planes + C ABI, Phase C/D/E
live-validated. #3 Bazzite packaging (`packaging/`) **deployed live** on a Bazzite F43 box (builds
against FFmpeg 7 **or** 8; gamescope capture → zero-copy NVENC, sub-ms latency; Sunshine replaced).
**Unified host:** `serve --native` runs the GameStream host + the punktfunk/1 QUIC host in one
process, with native pairing driven from the **web console** (arm → show PIN), not the service log.
Advanced DualSense (audio-driven voice-coil) haptics **scoped NO-GO** (`docs/dualsense-haptics.md`).
**Bazzite dynamic resolution (`c894c6f`):** the host now *manages* a headless `gamescope-session-plus`
Steam session at the **client's exact resolution + refresh** — games see it (via injected
`--nested-refresh` + generated CVT modes, not the box's TV EDID), relaunched per-connection on a mode
change, reused (no Steam restart) on the same mode. Plus macOS/iPad input fixes (NSEvent motion +
iPad pointer-lock) and a 4K/5K one-frame-freeze fix (grow the UDP socket buffers).

**Next:** **§8 pairing & trust hardening** (mandatory PIN by default + delegated approval), the M4
client presenter + iOS (§6), and a Windows host (§7 — now **de-risked via SudoVDA**, no custom
signed driver needed). **§10 HDR/10-bit is parked — blocked upstream at the compositor** (no
gamescope/KWin PipeWire 10-bit producer yet).

## 1. Reliable headless KDE/compositor spawning ✅ *(done — Phase 1 + 2)*

Startup is a chain of timing-sensitive handoffs with no readiness checks — each is a blind
`sleep`, one-shot timeout, or silent fire-and-forget that fails into a black screen.

- **Phase 1 (S):** replace `run-headless-kde.sh`'s blind `sleep 2` with an active readiness
  wait (kwin socket + `wl_display` roundtrip + `zkde_screencast` global advertised +
  KWIN_PID alive); add a `punktfunk-host probe-compositor` subcommand (reuses kwin.rs's
  registry roundtrip); move the portal restart to *after* readiness and precede it with
  `systemctl --user import-environment` + `dbus-update-activation-environment` (the missing
  env import — the Sway script does this, the KDE one doesn't).
- **Phase 2 (M):** bounded retry-with-backoff around `vd.create()` + first-frame
  (permanent vs transient); a PipeWire negotiation watchdog with zero-copy→CPU auto-fallback
  ("no PipeWire frame within 10s" → recovery or precise diagnosis); fix `set_custom_refresh`
  to wait for the output, read back the active mode, reconcile encoder fps; harden gamescope
  node discovery + detect the known-bad-gamescope signature; graceful PipeWire-thread stop.
- **Phase 3 (L):** supervised systemd user session (kwin + portal + host) with the readiness
  probe as an `ExecStartPost` gate, `Restart=on-failure`.

## 2. Offer available compositors in the client ✅ *(done)*

Host enumerates which backends are actually available (binary present + version OK:
gamescope ≥3.16.22, KWin ≥6.5.6, gnome-shell, sway), advertises the list in the punktfunk/1
Welcome + a mgmt-API field; client sends its pick in the Hello; host honors it per session.
Picker in the Apple client + web console.

## 3. Bazzite / install on other devices ✅ *(packaging written — `packaging/`)*

Bazzite already ships gamescope + PipeWire + the NVIDIA driver (incl. `libnvidia-encode`);
it's Fedora-atomic and the community installs Sunshine via COPR rpm-ostree — the analog.
Written: `packaging/rpm/punktfunk.spec` (builds the host from source), `packaging/bootc/Containerfile`
(`FROM bazzite-nvidia`), `packaging/bazzite/host.env` (gamescope default), `packaging/copr/` +
`packaging/README.md`. The build itself is operator-run (COPR / a Fedora toolbox; not buildable on
the Ubuntu dev box). `LICENSE-{MIT,APACHE}` added to match the declared dual license.

- **M-Bazzite-1:** a **COPR RPM** (primary) — binary + `60-punktfunk.rules` (→
  `/usr/lib/udev/rules.d`) + systemd `--user` unit + `host.env.example`; `Requires` the
  NVENC ffmpeg-libs Bazzite already pulls; links host `libcuda`/`libnvidia-encode` directly.
  Install = `rpm-ostree install` + reboot + add to `input`/`render`. Default backend =
  Bazzite's already-present **gamescope** (minimal session plumbing).
- **M-Bazzite-2:** wrap the RPM in a **bootc/OCI image layer** (`FROM
  ghcr.io/ublue-os/bazzite-nvidia:stable`) for the appliance/"just rebase" experience.
- Flatpak only later as an explicitly-degraded convenience build (sandbox fights
  zero-copy NVENC/dmabuf/uinput).

## 4. Mic passthrough — client mic → host input device ✅ *(done — host side)*

The exact mirror of the host→client desktop-audio path. A PipeWire virtual source apps can
select = a `pw_stream` with `Direction::Output` + `media.class=Audio/Source`.

- New `0xCB` MIC_AUDIO datagram (mirror of `0xC9`) + `NativeClient::send_audio` + ABI
  `punktfunk_send_audio`.
- `audio/source_linux.rs` — near-copy of the capture file, Direction::Output, fed from a
  jitter buffer (silence-fill underrun, Opus PLC).
- Host `mic_thread` (Opus decode → ring → source); teardown RAII, set `node.dont-reconnect`.
- Apple capture (AVAudioEngine → Opus). **Opt-in + paired-only** (a remote mic is a privacy
  surface). punktfunk/1-only.

## 5. Touch + rich DualSense *(decision: commit to full UHID DualSense)*

- **Touch — implemented (host path), pending a backend that lands it.** `TouchDown/Move/Up`
  InputKinds (reuse the abs-pointer `flags=(w<<16)|h` mapping, `code`=touch id); host
  `inject/libei.rs` requests the `Touchscreen` device type + binds the `Touch` capability and
  injects `ei_touchscreen` down/motion/up; `punktfunk-client-rs --touch-test` drags a finger.
  **Validated:** KWin's RemoteDesktop portal *grants* the Touchscreen device type, but its EIS
  server creates **no touchscreen device** (headless KWin) — so touch currently no-ops on KWin
  (now logged once). The code is correct; it needs a backend that exposes `ei_touchscreen`
  (gamescope / newer KWin / the real iPad client path) to land. wlroots: no virtual-touch wired.
- **Rich DualSense — HID backend built & validated live.** `inject/dualsense.rs`: a hand-rolled
  `/dev/uhid` codec (no bindgen) presenting a genuine USB DualSense (vendor 054C/0CE6, the 232-byte
  inputtino report descriptor) bound by the kernel `hid-playstation` driver. The mandatory
  GET_REPORT feature handshake (calibration 0x05 / pairing 0x09 / firmware 0x20) is answered, so the
  kernel creates the full device (gamepad/motion/touchpad/lightbar). Input report `0x01` is built
  from gamepad frames; output report `0x02` is parsed for LED RGB, player LEDs, and **adaptive
  trigger effects (L2/R2)**. Protocol carries new side-planes: rich-input `0xCC`
  (touchpad/motion) + HID-output `0xCD` (LED/triggers). `/dev/uhid` udev rule shipped.
- **Rich DualSense — Phase C/D/E end-to-end, validated live.** `PUNKTFUNK_GAMEPAD=dualsense`
  selects a per-session `DualSenseManager` (the `PadBackend` enum in `m3.rs`): client gamepad frames
  build the DualSense report; the kernel's feedback comes back as `HidOutput` on the **0xCD** plane
  (lightbar / player LEDs / adaptive triggers) while **rumble stays on the universal 0xCA plane**
  (so non-DualSense clients still feel it); touchpad + motion ride the **0xCC** rich-input plane
  (`DualSenseManager::apply_rich`, merged with button state). The connector + C ABI gained
  `punktfunk_connection_next_hidout` (→ `PunktfunkHidOutput`) and `punktfunk_connection_send_rich_input`
  (← `PunktfunkRichInput`); header regenerated. Validated on-box: a synthetic-source `m3-host` +
  `punktfunk-client-rs --rich-input-test` created the real kernel DualSense, drove 0xCC, and decoded
  12 live 0xCD events (the kernel's actual lightbar/trigger init reports) — data plane unaffected
  (600/600 frames). *Remaining:* the Apple client renders adaptive triggers + rumble on a real
  DualSense (`GCDualSenseAdaptiveTrigger`) — handed off to the client agent for the real playtest.
- **Advanced (audio-driven voice-coil) haptics — scoped, NO-GO for now (`docs/dualsense-haptics.md`).**
  Driven by the DualSense's USB *audio* interface (4-ch, back 2 channels = haptic PCM), not HID — so
  the UHID backend structurally can't carry it. Three independent walls: host capture needs a kernel
  rebuild (`CONFIG_USB_DUMMY_HCD` is off → no UDC for an `f_uac2` gadget); **near-zero Linux supply**
  (only ~5–10 Proton titles via custom Wine patches emit it; `hid-playstation`/Steam Input/RPCS3
  don't); and the Apple client can't faithfully replay PCM haptics (CoreHaptics is discrete/pattern-
  based, no public channel-3/4 routing). Deferred; revisit only if a real DS for capture + a UDC/host
  path + a PCM-capable client all land. Adaptive triggers (HID, above) deliver the reachable 80%.

## 6. iOS/iPadOS → tvOS *(deferred)*

PunktfunkKit is already platform-shared; iOS needs the `UIViewRepresentable` presenter twin
+ touch capture (#5) + UI. tvOS later.

## 7. Windows as a host *(scoped — `docs/windows-host.md`; de-risked via SudoVDA)*

Architecturally an "add a backend" job, not a parallel port: `punktfunk-core` (protocol/FEC/
crypto/C-ABI) + QUIC + GameStream + mgmt + the `m3`/pipeline orchestration are all platform-agnostic
and already `cfg`-isolated (~95% reuse). New `#[cfg(windows)]` backends behind the existing traits:
capture (DXGI Desktop Duplication / Windows.Graphics.Capture), encode (Media Foundation / NVENC-SDK
with a D3D11 context), input (SendInput + ViGEm), audio (WASAPI loopback + a virtual mic).

**The old blocker is gone.** Rather than author + sign our own kernel IDD for the per-client virtual
display, use **SudoVDA** (the Sunshine Virtual Display Adapter) — a pre-built, signed Indirect
Display Driver that creates virtual displays at arbitrary WxH@Hz on demand. The `VirtualDisplay`
backend becomes *"install + drive SudoVDA's control API"* (M effort), not *"write + WHQL-sign a
kernel driver"* (XL). That removes the only hard blocker — the Windows host is now a medium,
mostly-mechanical port. Recommended start: **Phase 0** — capture an existing monitor to prove the
stack end to end; **Phase 1** wires SudoVDA for the native-resolution output. Deferred only because
it's unbuildable on the Linux dev box; the trait boundaries are already in the right places.

## 8. Pairing & trust hardening *(next)*

The unified host + web-console pairing (arm a window → display the host PIN → user enters it on the
client) is built and live. Two changes harden it from "works" to "secure by default":

- ✅ **Mandatory PIN pairing by default — done & live** (`§8a`, `serve --native` now requires
  pairing; `serve --open` disables it). An unpaired client is rejected at the session gate; pairing
  is via the SPAKE2 PIN ceremony (one online guess, no offline attack) armed from the web console.
  Validated live: unpaired → "this host requires pairing", then web-armed PIN → "client trusted".
  Deployed to the dev box + Bazzite.
- **Delegated pairing approval** *(next — the ergonomic enabler for "mandatory": pair a device
  without fetching the host PIN out of band).* Target flow:
  1. Device A is already paired (authenticated) to Host X.
  2. The user tries to connect Device B to Host X.
  3. Host X surfaces a request: *"Allow Device B to pair with Host X?"*
  4. The user approves/denies; on approve, Host X admits Device B — binding B's certificate
     fingerprint — with no PIN typed.

  Two buildable layers:
  - **§8b-1 (host + web — achievable now):** an unpaired B that connects to an approval-enabled host
    is held as a **pending request** `{id, name, fingerprint, requested_at}` in `NativePairing`
    instead of a flat reject; mgmt gains `GET /native/pending` + `POST /native/pending/{id}/{approve,
    deny}`; the web console lists pending requests with Approve/Deny. The **operator approves from
    the console** — delegated approval via the management surface.
  - **§8b-2 (peer push — needs the client):** the host also pushes the pending request over a paired
    **Device A**'s live QUIC connection (a new control-plane message); A's app renders the prompt and
    replies approve/deny — the user's exact "Device A gets a notification" flow. The native/Apple UI
    is a client-agent task.

  PIN pairing (§8a) stays the bootstrap — the first device, or when no approver is online.

## 9. Client→host network speed test + settable bitrate *(host side done — client UI remaining)*

Measure what the network actually sustains so the bitrate picker is informed (suggest/cap a safe
value) instead of guesswork that ends in a stuttering stream.

**Done & live (host + protocol + connector + C ABI, `74819b1`):**
- **Bitrate negotiation**: `bitrate_kbps` rides Hello/Welcome (trailing-byte back-compat). The
  client requests a rate; the host clamps to [500 kbps, 500 Mbps] (or its 20 Mbps default on 0),
  applies it to NVENC (replacing the old hardcoded 20 Mbps) on the initial mode + every reconfigure,
  and echoes the resolved value. C ABI: `punktfunk_connect_ex3(…, bitrate_kbps, …)` +
  `punktfunk_connection_bitrate()`.
- **Bandwidth probe over the punktfunk/1 data path**: `ProbeRequest{target_kbps,duration_ms}` /
  `ProbeResult{bytes_sent,…}` control messages + a `FLAG_PROBE` packet flag. The host bursts
  zero-filled FEC-encoded AUs at the target goodput for the duration (clamped ≤ 1 Gbps / ≤ 5 s,
  video paused), reports what it sent; the connector measures received bytes/window → goodput + loss
  and exposes it (`punktfunk_connection_speed_test()` + `punktfunk_connection_probe_result()` →
  `PunktfunkProbeResult{throughput_kbps, loss_pct, …}`). Probe filler is diverted from the decoder.
  Validated on loopback (synthetic source): a 20 Mbps/2 s probe measured 20050 kbps at 0% loss,
  interleaved probe AUs excluded from frame verification. `punktfunk-client-rs` gains `--bitrate` +
  `--speed-test KBPS:MS` as the reference/loopback driver.

**Remaining (client UI):** wire the C ABI into the Apple client — a "Test network" action
(`speed_test` → poll `probe_result` → "~XXX Mbps · recommended bitrate YYY") feeding a bitrate
control (`connect_ex3`), and surface both in the web console.

## 10. HDR + 10-bit color *(parked — blocked upstream at the compositor producer)*

Opt-in HDR10 (BT.2020 + PQ, 10-bit) streaming. Designed end to end; **blocked at capture, not in our
stack** — the compositor doesn't emit a 10-bit/HDR PipeWire frame on any shipping build. Spiked +
researched 2026-06-11 (memory: `hdr-blocked-gamescope-pipewire`); the downstream design is ready to
build the moment a producer lands.

- **The wall — gamescope capture is 8-bit.** gamescope composites HDR for a *display*
  (`--hdr-enabled`, `--hdr-debug-force-output`), but its PipeWire capture node offers only `BGRx`/
  `NV12` (8-bit) — confirmed by reading `src/pipewire.cpp` `build_format_params()` on upstream master
  AND the box's exact build (`c31743d`); color is capped BT.601/709. Issue **#2126** ("pipewire: add
  HDR streams") is OPEN + unstarted (no PR). Forcing HDR output does not change the capture format.
- **PipeWire ≥1.6** is the other prerequisite (HDR colortype transport). Fedora 43 ships 1.4.x;
  Fedora 44 ships **1.6.6** — but Bazzite F44 `deck-nvidia` is **testing-only** (`:stable` is still
  F43; `:testing` has a confirmed NVIDIA Game-Mode crash). Rebasing now clears *only* the PipeWire
  wall while gamescope stays 8-bit → **no-go** for HDR; revisit a rebase when F44 promotes to stable
  (for its own sake), not for HDR.
- **The realistic route is KWin, not gamescope:** **KWin MR !8293** is a live draft adding HDR
  PipeWire capture. That pulls HDR onto the *desktop* (KWin) path — trading away gamescope's
  Steam-Deck-UI polish + the dynamic-resolution work (§ above). Track #2126 and !8293.
- **Constraints (settled):** NVENC tops out at **10-bit** → no Main12, no 12-bit AV1. **HDR ⟹ HEVC
  Main10** (Apple VideoToolbox decodes 10-bit HEVC but **not** 10-bit AV1). Static HDR10 SEI
  (BT.2020-PQ default) since the compositor won't surface per-frame metadata. Opt-in negotiation via
  the Hello/Welcome trailing-byte pattern (SDR default; client declares HDR want; host master toggle).
- **Downstream design (ready when capture unblocks):** add P010 + `ColorInfo` to capture; 10-bit
  zero-copy import (`GL_RGB10_A2`/float dest for RGB10, or P010 straight through the Vulkan→CUDA
  path); `hevc_nvenc -profile main10` + color/SEI metadata; opt-in Hello/Welcome + C ABI; Apple
  VideoToolbox Main10 decode + `wantsExtendedDynamicRangeContent` EDR present + SDR fallback.

## 11. 1 Gbps+ data plane *(foundation landed — the real work is batched/paced send)*

Support 1 Gbps+ video bitrate end to end — **the whole point of the GF(2¹⁶) Leopard FEC** (it breaks
the GF(2⁸)/Moonlight ~1 Gbps wall). A 6-way subagent investigation (2026-06-11) mapped every ceiling.

**Verdict: ~halfway, and it's mostly clamps + ONE real piece of work.** Already 1 Gbps-ready and
untouched: the integer/type path (u32 kbps → u64 → int64_t, no truncation); FEC (a 1 Gbps frame is
only ~434–874 data shards = a single GF(2¹⁶) block, two orders under the 65535 ceiling); AES-GCM
(RustCrypto auto AES-NI, ~10–25× headroom on x86_64); the u64 sequence/nonce space; and the **M1
`ReassemblerLimits`** — fully *derived* from the negotiated `FecConfig`, so they already admit every
legit high-bitrate frame with nothing to relax. Security invariant to keep: every allocation size
must trace to a host-negotiated parameter clamped to a scheme ceiling — scale via the negotiated
params (`max_data_per_block`, `shard_payload`), never by widening a bound by hand.

- **Done & live (`b8a33e2`) — make 1 Gbps configurable + its failure mode observable:** raised the
  clamps (`MAX_BITRATE_KBPS` 500 Mbps → 2 Gbps; `MAX_PROBE_KBPS` 1 → 3 Gbps so the probe can show
  headroom above the session cap); `TARGET_SOCKBUF` 8 → 32 MB (+ matching `99-punktfunk-net.conf`)
  so a multi-MB IDR burst doesn't fill the buffer; and surfaced the previously-silent WouldBlock
  send-buffer drop — `Transport::send` → `Result<bool>`, a new `packets_send_dropped` stat (Stats +
  C ABI `PunktfunkStats`), a `PUNKTFUNK_PERF` wire-Mbps/drop dump in `virtual_stream`, and the probe
  completion log. Loopback-verified the clamp no longer truncates a 1.2 Gbps probe.
- **The real bottleneck (next):** the native data plane is single-threaded with one `send()` syscall
  per packet — at ~125k pkt/s (1 Gbps wire) it burns a core on syscalls and mass-drops keyframe
  bursts. The fix is a **port, not invention**: lift the GameStream path's proven `sendmmsg_all`
  (64/call) + paced `spawn_sender` into the core `Transport` seam (`send_batch(&[&[u8]])`, Linux
  `sendmmsg`, scalar default), move FEC+seal+send onto a dedicated paced send thread, and mirror with
  `recvmmsg` + a reused buffer ring on the client (kills the per-recv alloc + the 300 µs-sleep
  underdrain). ~64× fewer syscalls.
- **Then refine as profiling shows:** add a FEC throughput-bench to `loss-harness`; reuse the
  reed-solomon engine in `Gf16Coder`; lower `max_data_per_block` 4096 → 256–1024 (bounds burst-drop
  blast radius + enables per-block FEC parallelism); seal in place via `AeadInPlace`; bump
  `shard_payload` 1200 → ~1452 (or jumbo after a path-MTU probe) for ~17% (or ~6×) fewer packets.
- **DoS hygiene (last):** derive the one hardcoded reassembler field (`max_frame_bytes` = 64 MiB,
  never set by `session_config`) from the negotiated mode/bitrate — strictly *tightens* the surface.
- **Validate with the speed-test probe** (it reuses the real `submit_frame`→FEC+crypto+send path):
  `punktfunk-client-rs --speed-test KBPS:MS`, RELEASE build (debug is CPU-bound ~30 Mbps), watching
  `packets_send_dropped`. Open Qs: NVENC CBR rate-tracking at 0.5–1 Gbps (no explicit
  `rc_buffer_size`); LAN/QEMU-NIC jumbo/GSO support; any `web/` bitrate slider hardcoding 500 Mbps.

## 12. Glass-to-glass latency *(investigated; quick wins landed, bigger bets scoped)*

A 5-way investigation (2026-06-11) mapped where latency actually lives. The measured "p50 0.83 ms"
is only the same-host **capture-stamp→reassembled** slice (~30–40% of true glass-to-glass) and was
measured with tiny single-chunk frames, so it excludes the pacing tail. The latency that matters, in
priority order: **(1) the host pacing tail** — `paced_submit` used to spread *every* multi-chunk
frame over ~90% of the interval (up to ~7.5 ms@120 / ~15 ms@60); **(2) native-path serialization** —
`virtual_stream` runs capture+encode+seal+paced-send on one thread, so frame N+1 can't start until
frame N's paced tail leaves the wire; **(3) client present** — `AVSampleBufferDisplayLayer` adds
~0.5 refresh (~4 ms@120Hz, ~8 ms@60Hz), the dominant client term at 60 Hz.

**Already optimal — do NOT touch** (confirmed): NVENC tuning (p1/ull/cbr/bf0/delay0/infinite-GOP +
forced-IDR — `receive_packet` is already same-frame); the device→device copy in `submit_cuda` (avoids
NVENC registration-cache thrash); FEC `max_data_per_block=4096` (every frame incl. a 4 MB IDR is one
block — no multi-block latency); the client reassembler (no jitter buffer, frame emitted on
last-packet arrival, `REORDER_WINDOW` is a dedup bound not a delay) — do **not** add a client jitter
buffer; `sendmmsg`/`recvmmsg` batching; the capture-timestamp anchor placement.

- **Done & live (`99f60b5`):** **microburst-cap pacing** — a frame ≤ a cap (default 128 KB,
  `PUNKTFUNK_PACE_BURST_KB`) bursts out immediately (no pacing tail); only a bigger frame's overflow
  (IDR / sustained high bitrate — the bursts that actually froze) is spread. Recovers the tail on the
  common case, keeps the freeze fix for the frames that need it; 128 KB is a safe default (well under
  the ~150 Mbps@60 frame size where drops began). Plus **per-frame instrumentation** (PUNKTFUNK_PERF):
  `encode_us` + `pace_us` p50/p99/max + immediate-vs-paced counts, so the cap is tunable against real
  numbers. **Validate with the LAN soak before raising the cap** (`send_dropped` must stay 0).
- **Done & live (`b295a5b`; validated on the GNOME box 2026-06-12):** **encode|send thread split**
  on the native path — a dedicated `send_loop` thread owns the `Session` and does seal+pace+send+
  probes; the encode thread captures+encodes+handles reconfig and hands `FrameMsg` over a bounded
  `sync_channel(3)` with backpressure. Removes the serialization (~2–8 ms @60–120 fps) and is the
  substrate the slice wrapper needs. Real-NIC soak (host on the Ubuntu/GNOME box, client over the
  LAN): `send_dropped=0` at 720p60 / 1080p120, and a 1 Gbps probe pushed 625 MB in 5 s clean.
- **Done & live (skew handshake landed 2026-06-12):** **wall-clock skew handshake** — `ClockProbe`/
  `ClockEcho` on the control stream (8 NTP-style rounds right after `Start`; min-RTT sample →
  host−client offset; `clock_offset_ns`). The client adds the offset to its receive instant before
  differencing against the AU `pts_ns`, so the `capture→reassembled` percentiles are now valid
  **across machines** (reported `skew_corrected=true`), not just same-host. Back-compat: an old host
  that doesn't answer times out → `skew_corrected=false` (shared-clock assumption, as before).
  Validated cross-LAN (GNOME box → dev box): offset ≈ −1.57 ms (reproducible), rtt ~140 µs, **p50
  1.30 ms** skew-corrected capture→reassembled. The skew handshake is now a shared core helper
  (`quic::clock_sync` → `ClockSkew`) used by both the reference client and the **embeddable
  connector** — `NativeClient` runs it at connect and exposes the offset over the C ABI
  (`punktfunk_connection_clock_offset_ns`), so the Apple client can convert a present instant to the
  host clock. **Remaining for true glass-to-glass**: (1) the **Apple client present-stamp**
  (decode→present) — Swift: stamp `AVSampleBufferDisplayLayer`/presenter time, add the C-ABI offset,
  subtract the AU `pts_ns`; (2) the host **render→capture** term (PipeWire buffer presentation
  timestamp vs our capture stamp). `tools/latency-probe` is still the cross-machine orchestrator.
- **Bigger bets (ordered, deferred — need real-NIC/GPU/Mac validation):**
  1. **CUDA stream+event** to drop one of two redundant `cuCtxSynchronize` in `submit_cuda` (keep the
     copy) — ~0.1–0.4 ms@720p, ~1 ms@5K; only if per-stage timing proves the sync is on the path.
  2. **Stage-2 Apple presenter** (`VTDecompressionSession` → `CAMetalLayer`, hand-paced) — ~0.5 refresh
     off the present tail (biggest client win at 60 Hz); gate on the probe proving present is real.
  3. **NVENC slice-mode wrapper** (roadmap §2 sub-frame pipelining) — per-slice transmit overlaps
     encode+send within a frame (~3–6 ms at 4K/5K/IDR); large + driver-ABI-fragile, on top of the
     thread split, only after measurement justifies it.

## 13. Native-protocol LAN auto-discovery ✅ *(done — 2026-06-12, validated cross-LAN)*

The native protocol had no discovery — clients connected by `--connect HOST:PORT` only, while
GameStream already auto-discovered via mDNS (`_nvstream._tcp`). Now both the unified host
(`serve --native`) and standalone `m3-host` advertise the native service over mDNS:

- **Service**: `_punktfunk._udp.local.` (UDP — punktfunk/1 is QUIC; the advertised port is the QUIC
  control/data port). Host side: `crate::discovery::advertise_native`, wired into `m3::serve` so
  both host entry points get it; best-effort (a discovery failure never blocks streaming —
  `--connect` always works). The advert is held for the host's lifetime (RAII unregister).
- **TXT records**: `proto=punktfunk/1`, `fp=<host cert SHA-256>` (the value a client pins — advisory
  over unauthenticated mDNS, TOFU/pinning still verifies on connect), `pair=required|optional`
  (so a picker knows up front whether the PIN ceremony is needed), `id=<host uniqueid>` (dedup).
- **Client**: `punktfunk-client-rs --discover [SECS]` browses and prints each host (name, addr:port,
  pairing, fingerprint), then exits. Apple clients browse the same service natively via NWBrowser
  (Bonjour) — no Rust-connector dependency; this section's service type + TXT keys are the contract.
- **Validated**: cross-LAN — dev box discovered the GNOME-box appliance
  (`home-worker-3 192.168.1.248:9777 pair=required fp=1dcf3a…`) and a standalone synthetic host
  (`pair=optional`); fingerprint + pairing state correct in both.
- **Next** (not done): wire NWBrowser discovery into the Apple client UI (host picker); the
  host-side contract above is all it needs.
