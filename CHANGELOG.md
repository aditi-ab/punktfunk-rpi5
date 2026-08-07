# Changelog

Protocol, ABI, driver and embedder detail, one section per stable release, newest first.

This is the **technical** half of a release. The other half — what changed for people who *use*
Punktfunk — is `docs/releases/vX.Y.Z.md`, and it deliberately contains no internal names. The two
were one document through v0.24.0; they split at v0.25.0 because the engineering section had grown
long enough to bury the user-facing half it was appended to. See `docs/releases/README.md`.

If you embed `punktfunk-core`, package Punktfunk, or write a plugin, this file is for you. Start
with the version table of the release you are moving to, then read **Breaking changes**.

---

## v0.25.0

327 commits since v0.24.0.

### Versions

| | v0.24.0 | v0.25.0 | Notes |
|---|---|---|---|
| Wire protocol | 2 | **2** | unchanged — every addition below is optional or capability-gated |
| C ABI | 14 | **17** | three steps; see below |
| Workspace crate dirs | 22 | **26** | `pf-bitstream` (+ vendored `cros-codecs`), `pf-vkdecode`, `pf-dxvadec`, `pf-vaadec` added; `pf-ffvk` removed |
| Virtual-display driver protocol | 6 | **6** | unchanged (minimum accepted still 3) |
| Windows virtual-gamepad channel | 3 | **3** | unchanged |
| Plugin index schema | 1 | **1** | unchanged |
| `api/openapi.json` | 0.23.0 | **0.24.0** | tracks API edits, lags one release by convention |

`crates/pf-driver-proto` is byte-for-byte identical to v0.24.0 — if you ship the virtual-display
driver or the gamepad channel, nothing in this release touches you.

**Why the wire did not move.** It grew a lot and still did not break: an optional trailing
`max_shard_payload: u16` on `Hello` (absent/0 = legacy, doubling as the renegotiation capability
flag and the jumbo receive ceiling); two control messages `ShardPayloadChanged` (`0x08`) and
`ShardPayloadAck` (`0x09`); a redundant desktop-audio datagram tag `0xD2` beside the plain `0xC9`; a
controller-audio plane at `0xD1`; a new `0xCD` kind `0x06`; arrival flag bits 8/9; and
`MAX_DATAGRAM_BYTES` 2048 → 9216. Old peers never send or read any of it. Bump `WIRE_VERSION` only
when the handshake or planes change *incompatibly* — riding a C-ABI bump onto the wire once locked
every new client out of every deployed host (`ABI mismatch: client 3 host 2`, observed live).

### C ABI 14 → 17

- **v15 — the rumble policy engine's C surface.** `punktfunk_connection_next_rumble_cmd`,
  `punktfunk_connection_set_rumble_quirks`, `PUNKTFUNK_RUMBLE_QUIRK_*`. These symbols are **not
  new**: they landed while the constant still read 7 and no bump was made, so every core since has
  exported them while advertising a version that never promised them. A shipped binary says what it
  says, so this cannot be corrected retroactively — **v15 is the floor that guarantees them.** At or
  above 15 the surface is present; below it, probe for the symbol. No code changed with this bump.
- **v16 — the controller-audio client surface.** `punktfunk_connection_next_pad_audio` (the `0xD1`
  per-gamepad DualSense haptics/speaker plane), `punktfunk_connection_set_pad_audio_caps`, and the
  `PUNKTFUNK_CLIENT_CAP_PAD_AUDIO` / `PUNKTFUNK_HOST_CAP_PAD_AUDIO` mirrors.
- **v17 — session end reason.** `punktfunk_connection_end_reason` + the `PUNKTFUNK_END_REASON_*`
  vocabulary: after a session ends, ask *why* — this client closed it, the host's launched game
  exited (its close carried `APP_EXITED_CLOSE_CODE`, which the host had been sending for a long time
  with nothing consuming it), the host ended it cleanly, the host reported a failure, or the
  connection was lost. Purely a read of state the core already had: **no new call is required of an
  embedder**, a client that never calls it is unchanged, and the host sends identical bytes either
  way.

### ⚠ Breaking changes

**1. 149 unprefixed macros are now `PUNKTFUNK_`-prefixed** (139 `#define`s renamed in the checked-in
header). Names as generic as `MAX_PADS`, `TAG_LEN`, `ABI_VERSION`, `WIRE_VERSION`, `INPUT_MAGIC` and
the whole `BTN_*` / `AXIS_*` family were landing in the namespace of every program that included the
header.

*What to do:* add the prefix. Values are identical; the change is mechanical.

*It cannot break silently.* The old spellings cease to exist, so this is always an
undeclared-identifier error, never a wrong value — which is precisely the failure being removed. A
colliding `#define` does **not** fail to compile: the preprocessor silently takes the last
definition, so an embedder whose own header defined `MAX_PADS` previously got a wrong value at
runtime. Associated constants are untouched; the generator already qualifies those by type name.

**2. Linux hosts: the virtual Steam Deck controller moved to its own `punktfunk` group.** The
capability rode on `input`, which every gamepad guide tells users to join — but it can emulate
arbitrary USB hardware. Operators must `usermod -aG punktfunk "$USER"` and re-login or the pad stops
attaching. Ordinary virtual gamepads are unaffected.

**3. Plugins may no longer set `launch.command` or the pre-launch command.** Both run through a
shell and are now operator-token only; a plugin that sets them is refused. Third-party plugins that
populated them need updating — use the `launcher_ui` / `xbox` launch kinds instead.

**4. Plugin UIs moved to their own origin** on a second listener (default `PORT + 1`,
`PUNKTFUNK_UI_PLUGIN_PORT`). Reverse proxies and firewalls must forward that port; a self-signed
console needs it trusted separately.

### Capability bits

Four added, all in the handshake's client/host capability bytes:

| Bit | Constant | Meaning |
|---|---|---|
| client `0x04` | `CLIENT_CAP_AUDIO_RED` | can decode the redundant desktop-audio plane |
| host `0x20` | `HOST_CAP_AUDIO_RED` | is sending it |
| client `0x08` | `CLIENT_CAP_PAD_AUDIO` | can render controller audio |
| host `0x40` | `HOST_CAP_PAD_AUDIO` | is sending it |

⚠ **Pressure worth watching:** `client_caps` has four bits free; **`host_caps` is down to its last
one (`0x80`)**; `video_caps` has been full since 0.23.0 (`VIDEO_CAP_MULTI_SLICE = 0x80`). The next
video capability needs a second byte *and* an ABI bump — plan for it rather than discovering it.

### Wire planes

- **Controller audio, `0xD1`** — `[0xD1][u8 pad][u8 kind][u32 seq LE][u64 pts_ns LE][opus payload]`,
  one Opus frame per datagram behind a 15-byte header. `PAD_AUDIO_KIND_HAPTICS = 0` is the pad's
  BACK channel pair (the voice coils) at 5 ms frames; `PAD_AUDIO_KIND_SPEAKER = 1` is the FRONT pair
  at 10 ms. Best-effort like every audio plane: loss is a sequence gap concealed by the gap tracker,
  silence is a frozen sequence under the mic-mute discipline, host gating at −60 dBFS with a 250 ms
  hangover. `0xD2` (redundant desktop audio) deliberately skipped `0xD1` to reserve it for this.
- **`HidOutput::AudioCtl`** — `0xCD` kind `0x06`, carrying the DualSense output report's
  volume/routing bytes, change-only and value-deduped. Older clients drop it as an unknown kind.
- **Arrival flags** — bits 8 (haptics) and 9 (speaker), sent only toward a `HOST_CAP_PAD_AUDIO` host.
- **Adaptive-trigger effects are length-bounded** on encode and decode against one shared constant;
  the header emits `uint8_t effect[PUNKTFUNK_HID_EFFECT_MAX]` in place of a literal `11` (same value,
  so the struct layout is byte-identical). A zero-length effect body is now rejected rather than
  decoding as an empty — that is, a *release* — effect.
- Out-of-range pad indices are dropped before **either** rumble consumer sees them. The reorder gate
  bounds-checked and the legacy queue did not, so an embedder draining it could be handed an index it
  would use to subscript its own array. The client also clamps the host's rumble lease receive-side
  at 5 s, where the ceiling had been sender-side only.

### Host environment variables

| Variable | Default | Notes |
|---|---|---|
| `PUNKTFUNK_AUDIO_QUALITY` | `high` | `low`/`standard`/`high`; `high` = stereo 256 kbps. `standard` reproduces the pre-0.25 encoder exactly for an A/B. A typo warns once rather than silently downgrading. |
| `PUNKTFUNK_AUDIO_REDUNDANCY` | unset = automatic | on when the client supports it and the budget allows |
| `PUNKTFUNK_AUDIO_OUTPUT_MODE` | `client_only` | `client_only`/`host_and_client`/`follow_default`. **Windows host only.** |
| `PUNKTFUNK_PAD_AUDIO` | on | `0` disables controller audio host-wide |
| `PUNKTFUNK_PAD_AUDIO_SLOTS` | `1` | max 4; multi-pad needs an operator to raise it |
| `PUNKTFUNK_PAD_AUDIO_STAMPS` | unset | debug bisect hook |
| `PUNKTFUNK_WIRE_MTU` | unset | pins on-wire IP MTU for all sessions; above 1500 also enables jumbo |
| `PUNKTFUNK_JUMBO` | unset (off) | fixed 9000-MTU profile |
| `PUNKTFUNK_UI_PLUGIN_PORT` | `PORT + 1` | the plugin-UI origin |
| `PUNKTFUNK_LIBRARY_ART_ROOTS` | platform default | art-serving roots; POSIX now defaults to `$HOME` |
| `PUNKTFUNK_DECODER` | client | **values changed**: `native-vulkan` · `native-vaapi` (Linux) · `native-d3d11va` (Windows) · `software`. Legacy `vulkan`/`vaapi`/`d3d11va` still accepted and migrated. Now **trimmed** — a trailing space used to fall through to `auto` silently. |
| `PUNKTFUNK_VAAPI_DEVICE` | client | **new** — pin the VAAPI render node |
| `PUNKTFUNK_DUMP_VIDEO` / `PUNKTFUNK_AU_DUMP` | client | **new** — capture exact decoder input / the AU as it arrived from the host |
| `PUNKTFUNK_AU_FAULT=drop\|truncate\|flip[:period]` | client | **new** — deliberate decoder-input corruption for recovery testing; native rungs only |
| `PUNKTFUNK_NVENC_SPLIT_ARBITRATE=1` | host | **new** — opt-in live split-encode arbitration (Linux-wired) |
| `PUNKTFUNK_NO_AUDIO_MINT` | host (Win) | **new** — opt out of minted endpoints; restores the name ladder |
| `PUNKTFUNK_GPU_PRIORITY` | host (Win) | **removed** — superseded by `PUNKTFUNK_GPU_PRIORITY_CLASS`, a strict superset |
| `PUNKTFUNK_FFMPEG_LOG` | client | **removed** with the av_log machinery |

Legacy `PUNKTFUNK_HOST_AUDIO=1` and `PUNKTFUNK_KEEP_DEFAULT=1` still work, mapping to
`host_and_client` and `follow_default`; `follow_default` wins if both are set. New devtest command:
`punktfunk-host pad-endpoint ensure|remove|status`.

### Security

- **Origin isolation.** A second listener serves `/plugin-ui/**` and nothing else; the console origin
  refuses those paths and the plugin origin refuses everything else, `/api/**` above all. Different
  origin (scheme+host+port) so same-origin policy *is* the boundary; same site so the `SameSite=Lax`
  session cookie still flows. Bind failure disables plugin UIs rather than falling back.
  `x-pf-listener` is stripped inbound and set by the entry; active ports republish as
  `*_PORT_ACTIVE`; the plugin origin's CSP names the console as its only `frame-ancestors`; the proxy
  allowlist drops the plugin's `Clear-Site-Data`, `Access-Control-Allow-Origin` and `Set-Cookie`.
  ⚠ The kit's `postMessage(..., "*")` is **load-bearing** — narrowing it to `location.origin` would
  target the plugin's own origin and drop every message.
- **Authorization is an allowlist with a build-time gate.** `plugin_may_access` is a list of
  permitted `(method, path)` pairs with `{}` segment matching, enforced by a test that walks the live
  route table and **fails the build on any unclassified route** — the block-list it replaces let new
  endpoints through silently. Field authority is tracked separately from route reachability:
  requests carry the lane that authorized them, and `prep` / `launch.kind = "command"` are
  operator-token only.
- **Art serving** gained an extension whitelist plus magic-byte sniffing, canonicalize-or-refuse, UNC
  refusal, config-dir exclusion and root checking, with `file://` percent-decoded *before*
  canonicalization so `%2e%2e` cannot hide. Validation also runs at write time, so an unservable path
  can no longer be persisted.

### Native decode — FFmpeg is gone from the client

268 files, +129k / −25k. `cargo tree -p punktfunk-client-session` finds zero `ffmpeg`. **The host
keeps `libavcodec` unconditionally** (pf-encode); no host workflow, packaging script or licence file
was touched.

| Platform | v0.24.0 | v0.25.0 |
|---|---|---|
| Linux desktop | ffmpeg-next: Vulkan hwcontext (`pf-ffvk`) → VAAPI → libavcodec sw | `pf-vkdecode` (ash, presenter's own `VkDevice`, zero-copy) → `pf-vaadec` (dlopen'd libva, DRM-PRIME dmabuf) → `openh264` + `rav1d` |
| Windows desktop | ffmpeg-next Vulkan → libavcodec D3D11VA half | `pf-vkdecode` → `pf-dxvadec` (plans into `ID3D11VideoDecoder`) → `openh264` + `rav1d` |
| Android | MediaCodec (never had FFmpeg) | unchanged |
| Apple | VideoToolbox (never had FFmpeg) | unchanged |

**Workspace members:** added `pf-bitstream` (+ vendored `cros-codecs`, compiler-enforced
`unsafe`-free), `pf-vkdecode`, `pf-dxvadec`, `pf-vaadec`; removed `pf-ffvk`. **Deleted:**
`video_vulkan.rs`, `video_vaapi.rs`, `video_libav.rs`, the libavcodec half of `video_d3d11.rs`, the
`av_log` machinery, `ffmpeg::codec::Id` as decoder vocabulary, `DecodedImage::VkFrame`/`::Dmabuf`,
the `ffmpeg-fallback` feature, and swscale — and with it the BT.601 default its correction code
existed to undo.

**Software rung:** `openh264 = "0.9"` (BSD-2) and `rav1d = { version = "1", default-features =
false, features = ["bitdepth_8"] }` (BSD-2). `dav1d-sys` was rejected because it is `system-deps`-
only and would add a system library plus a `.pc` to every client package. `default-features = false`
drops `asm` — rav1d's `build.rs` *panics* without nasm, unlike openh264-sys2, which degrades quietly.
**`bitdepth_8` only** ⇒ software AV1 refuses 10-bit by contract, read from the sequence header before
any byte reaches the decoder.

**⚠ HEVC has no CPU floor.** An HEVC session that exhausts its hardware rungs tears down and re-dials
advertising HEVC-less caps, and the host picks H.264 (`last_rung_verdict` / `NoSoftwareRung`). This is
a first-class path, not a failure.

**Rung × codec × hardware evidence** (`native_evidence`) — the admission filter is driven by this, so
an unproven rung yields only to one that is both verified for the codec and usable on the device:

| Rung | Codecs | Evidence |
|---|---|---|
| `native-vulkan` | H.264, H.265 Main/Main10/4:4:4 | **yes** — bit-exact vs libavcodec, 250/250 AUs on 3 drivers + 92-min soak |
| | AV1 | **yes** — 250/250 bit-identical on one vendor, no soak |
| `native-d3d11va` | H.264, H.265 | **yes** — frame-hash parity on RTX 4090 + AMD iGPU, 30-min soak |
| | AV1 | **not proven** — decoded 4K60 once, no parity, no soak ⇒ excluded from the filter |
| `native-vaapi` | H.264, H.265, AV1 | **NO — has never decoded a frame anywhere**; no VAAPI hardware was reachable |
| `software` | H.264 (openh264), AV1 (rav1d) | **not proven**; openh264 has never run on glass. No HEVC at all. |

Vendor order (unchanged): Linux NVIDIA/AMD `vk → vaapi → sw`; Linux Intel/unknown
`vaapi → vk → sw`; Windows NVIDIA/AMD `vk → d3d11va → sw`; Windows Intel/unknown
`d3d11va → vk → sw`.

**AV1 advertisement** now answers from device facts (`av1_hardware_decodable`: Vulkan `DECODE_AV1`
queue op, or the Windows D3D11 import path) rather than `ffmpeg::decoder::find(AV1)`, which was true
on any build linking libdav1d. **Settings migration:** stored `vulkan`/`vaapi`/`d3d11va` migrate to
`native-*` at decoder construction *and* at each dialog's lookup — the second is load-bearing, since
an unmatched value renders as "Automatic" and a save would silently rewrite the preference.

### The three decode data-loss bugs

**AV1 sub-frame truncation — shipped in v0.24.0, host-side.** NVENC sub-frame readback has two halves
armed by *different* conditions: `build_init_params` arms the writer from `subframe_on` alone, while
the chunked reader additionally requires `slices >= 2` — and `resolve_slices` returns `1` for AV1
unconditionally, because AV1 partitions via tiles, not slices. So an AV1 session told the driver to
publish tile-by-tile and then took only the first tile. Measured at 4K60: every AU carried a header
declaring two tile rows plus a single Tile Group OBU with `tg_start = tg_end = 0`; libdav1d rejected
**835/836** AUs. NVIDIA's *hardware* decoder accepts it (so Vulkan Video looked healthy at 60 fps);
its DXVA path did not. 1080p is one tile and unaffected; 4K splits into two tile rows and loses half
the picture. Fixed by disarming sub-frame for AV1 while leaving `split_mode` untouched — AV1 keeps
every engine. Arming the reader instead is *not* a drop-in: the reader cuts at
`bitstreamSizeInBytes` on the reasoning that slices are contiguous Annex-B, which AV1 OBUs are not.
Post-fix 654/654 clean. The test that had pinned the old behaviour as *correct* is replaced by one
pinning the disarm, plus one comparing the reader's gate against the writer's — the comparison
nothing made.

**HEVC DPB from the level ceiling — new in this release, client-side.** `dpb_limit` computed
`max(A-2_level_ceiling, sps_max_dec_pic_buffering_minus1 + 1)`. HEVC equation A-2 is a **ceiling on
what an SPS may legally signal**, not a statement of need, and it branches on picture size against
the *level's* `MaxLumaPs`. The host is blameless: NVENC autoselects L5.1 because the bitrate exceeds
L5.0's ceiling, and signals six pictures at every resolution. At 720p and 1080p the A-2 branch yields
16 frames / **17 slots** — one more than NVIDIA's `maxDpbSlots` of 16 — so every AU fell outside
device caps, flushed, waited for an IRAP, and the fresh IDR needed 17 again; rungs exhausted, and
there is no software HEVC. It hid because the path was only ever exercised at 4K, the one size that
falls through to the honest answer. Fixed to `buffering.min(16)`: the `max()` bought no tolerance,
since `Dpb::needs_bumping` already evicts at the signalled depth — it only over-allocated ten
surfaces per 1080p session. **H.264 escaped by luck** (its ceiling lands at 13 for 1080p) and is left
alone, because H.264's DPB size genuinely *is* level-derived absent a VUI `bitstream_restriction`.

**rav1d aborts the process — new in this release, client-side.** rav1d 1.1.0 `abort()`s on *any*
decode error while holding one frame context: the `c.fc.len() == 1` branch decodes inline, always
finishes in `rav1d_decode_frame_exit` which unconditionally takes `frame_hdr`, then on `Err` re-enters
an `on_error` whose first act is `frame_hdr.as_ref().unwrap()` on the `None` it just left. The panic
unwinds into `dav1d_send_data`, which is `extern "C"` ⇒ `panic_cannot_unwind` ⇒ `abort()`. **No
`catch_unwind`, no rung demotion and no refusal can catch it**, and every `rav1d_*` entry is
`pub(crate)`, so no in-process guard is possible. 4K was only *where* the first error happened — the
CPU rung does 35–39 fps against a 60 fps stream, the backlog stopped draining, the pump flushed to
live, and the next AU referenced undecoded frames. Fixed by opening with `n_fc >= 2` and asking
`dav1d_get_frame_delay` what the settings actually bought. Decode now drains **past** the first
`EAGAIN`, which is why two frame contexts cost no latency (20–42 ms/unit at `n_fc=2` vs 21–53 at
`n_fc=1`). On glass: 4K60 AV1 was SIGABRT on the second frame every run; after, exit 0 with 1204
frames and 13 decode errors recovered across 17 backlog flushes. Reported upstream as **rav1d#1497**
with a reproducer. Does **not** make the CPU rung panic-proof.

**Settings loader BOM — shipped in v0.24.0, client-side.** `.and_then(|s| from_str(&s).ok())` turned
every parse failure into `Default`. `Set-Content -Encoding UTF8` writes `EF BB BF`, serde_json
correctly rejects at byte 0, and every setting vanished silently. A shared `load_json_or_default` now
strips the BOM and warns with path plus serde line/column, covering settings, known-hosts (where a
BOM silently unpaired every host) and profiles on both desktop clients. The result is deliberately
still `Default`, never an error.

### Other decode/encode

- **Intel Arc pNext ordering.** `vkGetPhysicalDeviceVideoCapabilitiesKHR` was called with the codec
  caps struct chained *before* `VkVideoDecodeCapabilitiesKHR` (`push_next` prepends). Arc/Windows
  fills those two **by position, not by sType**, and returned them swapped — we read a level as a
  capability bitmask. Measured A/B: `decode_flags_raw=12 max_level_idc=1` before,
  `decode_flags_raw=1 max_level_idc=12` after. NVIDIA and RADV dispatch by sType, which is why the
  fleet stayed green. ⚠ **This does not yet give Arc Vulkan Video** — the refusal only moves down: the
  device advertises only COINCIDE, and its NV12 coincide entry does not advertise `SAMPLED` usage,
  which the zero-copy presenter needs. Unresolved whether that is ours or an Intel constraint.
- **NVENC split encode.** The 10-bit rule sat *above* the pixel-rate arm and took no codec, so it
  vetoed 10-bit 4K120 — the exact case the pixel-rate arm exists for — and applied an
  HEVC-Main10-on-Ada result to AV1 10-bit, which has no such measurement. Re-measured on Ada and
  Blackwell: 4K60 2.06×, 5120×1440@240 1.31×, 4K120 1.89× — **split wins at every mode on both
  architectures, including the configuration the veto came from.** New order: env override →
  pixel-rate arm (now taking `max_forced_split_mode(engines)`, not a hard-coded 2) →
  HEVC-Main10-below-the-bar → AUTO. Operator over-asks are clamped with a warning because **the driver
  honours an over-ask and silently encodes narrower**. Also newly logged: HEVC + plain AUTO +
  sub-frame is **silently single-engine** — the fleet's default shape, and nothing said so.
  ⚠ **Unvalidated consequence:** 5120×1440@240 Main10 now clears the pixel-rate bar and *will* be
  forced to split — the exact configuration the old veto came from. `PUNKTFUNK_SPLIT_ENCODE=0` is the
  escape.
- **PyroWave on Windows stamped over the host's GPU scheduling policy.** It raised the process WDDM
  class to HIGH at every session open, while `auto_priority_gate` already owns that process-wide —
  starting at HIGH, *upgrading* to REALTIME once safe, and leaving a monitor that drops back when VRAM
  tightens (REALTIME + NVIDIA + HAGS + near-full VRAM is a documented NVENC hang). Opening PyroWave
  stamped HIGH back and **orphaned the monitor's decision**. Removed rather than reconciled.
- **A `pf-vkdecode` AV1 use-after-free fix had stabilised the wrong pointer** —
  `OwnedStdAv1SequenceHeader` kept the Std struct *inline*, so `pStdSequenceHeader` was a dead stack
  address; it worked only because NVIDIA happened to retain `pColorConfig` instead. Std structs are
  now boxed inside each owning wrapper, and create-time arrays are fields of the stored parameters
  assembled at their final address. The same shape was fixed pre-emptively in H.264/H.265.

### Windows audio substrate

The host now mints its **own** devnodes from Valve's INFs (`SteamStreamingSpeakers.inf` /
`SteamStreamingMicrophone.inf` under `{CommonProgramFiles(x86)}\Steam\drivers\Windows10\…`) instead
of bundling VB-CABLE.

- **Two persistent endpoints**, `Punktfunk Speakers` (client-only loopback sink — the wiring plan
  parks the default playback on it during a stream, its WASAPI loopback feeds the encoder, the host
  stays silent) and `Punktfunk Microphone` (host writes decoded client voice into the render side;
  the capture side surfaces as the mic). Both survive host restarts and re-resolve by marker.
- **Identity is the recorded endpoint id, never the name** — a minted instance is name-identical to
  Steam's primaries. Durable marker `PunktfunkAudioRole` (1 = Speakers, 2 = Mic) under Device
  Parameters. Name stamping is device-desc + device-name **only**: a wider stamp set makes
  `AudioEndpointBuilder` re-mint under a new GUID. Best-effort via the SYSTEM ACL route; on failure
  the endpoint still wires and simply keeps the driver's default name.
- **Format stamps are per-direction.** Render gets the PCM16-device / float-mix stereo split; capture
  gets the **device-format key only** — mix and host-format keys are render-engine properties, and
  stamping them onto a capture endpoint breaks its shared-mode graph (`IsFormatSupported` reports
  2ch/48k fine, `Initialize` then fails `0x88890008`).
- **`MintedIds` is tier-0 in the wiring plan.** The mic takes its minted device outright (paired by
  provider id — a name search cannot distinguish it from the primary); the loopback prefers the
  minted sink at the head of the silent tier. Below that the old ladder is unchanged: Steam primaries
  → cable → real hardware. `PUNKTFUNK_MIC_DEVICE` still beats everything.
- **Mic-vs-loopback arbitration**: the mic may hold the Streaming Microphone only while the loopback
  still gets a non-last-resort pick; otherwise the loopback takes it and `mic_withheld` is set. This
  fixes a field case where a headless Steam-only host streamed **silence**.
- **New `AudioReadiness`** — `Full` / `AudioOnly` / `MicOnly` / `Nothing`, logged on every plan
  change and surfaced at `GET /api/v1/status` → `RuntimeStatus.audio` (`AudioWiring`, Windows-only,
  absent before the first wiring pass; a status poll triggers no COM work or `IPolicyConfig` writes).
  The console Dashboard renders it as an "Audio wiring" card.
- **Requires Steam installed** (never running) — without the INFs the host streams video only, and
  picks the drivers up automatically if Steam is installed later. Opt out entirely with
  `PUNKTFUNK_NO_AUDIO_MINT`, which restores the previous name-based ladder exactly.
- ⚠ **VB-CABLE is no longer bundled but is deliberately NOT uninstalled** — it is a third-party
  shared component other apps may use, and it stays in the ladder as a live fallback. Demoting it was
  considered and rejected: on a box where minting transiently fails, that would let the Steam
  Streaming Microphone outrank an installed cable, steal the silent sink and make stream audio
  audible on the host.
- ⚠ **The minted endpoints survive Punktfunk's uninstall by design** (they are plain instances of
  Steam's drivers and are inert without the host). There is no user-facing removal path; cleanup is
  the devtest `punktfunk-host audio-probe cleanup`.
- New devtest: `punktfunk-host audio-probe ssm|sink|sss-primary|mint|plan|micpitch|micpins|cleanup`.
  `plan` is the field-triage command; `micpins` maps exclusive+shared `IsFormatSupported` across
  {1,2}ch × {16,32}bit × {44.1,48,96}kHz on both mic pins.

### Apple audio

- **The microphone was never in the render graph.** On the combined (voice-processing) engine — made
  default a week earlier and never run on a device — the input node carried a tap and **no
  connection**, so nothing pulled it: the IO unit came up, the recording indicator lit for a beat,
  and not one buffer ever reached the tap, with no error and no failed start. The 10 s silence
  tripwire counts *captured* frames, so it never fired. Input now runs through a silent sink into the
  main mixer at `outputVolume = 0` (Apple's own voice-processing sample topology). Two more: the tap
  read the input format **before** `prepare()`, and enabling voice processing swaps in the VPIO unit
  and renegotiates, so the pre-swap read could be 0 Hz / 0 ch; and a mic-chain failure on the
  voice-processed engine took the whole uplink down for the session — it now falls back to the split
  path, because **the mic outranks the AEC**.
- **No packet-loss concealment on the one client that decodes Opus in core.** Linux, Windows and
  Android all feed an `AudioGapTracker` and synthesize libopus PLC; the in-core path had the tracker
  sitting unused in the same crate and decoded only packets that arrived. At ~200 packets/s of 5 ms
  frames every lost datagram was a hard time-domain gap — one click per loss. The redundant plane
  (`0xD2`) hides single losses, so the survivors were exactly the burstier gaps that most needed
  concealing. Concealed frames now land in front of the arriving frame in one contiguous buffer, a
  DTX marker advances accounting without being decoded, and the output buffer is pre-sized for a full
  concealment run so the borrow-until-next-call pointer cannot dangle (50 ms cap).
- **The Apple jitter ring never grew.** The shared Rust `JitterPolicy` has an adaptive target floor;
  the hand-written Apple mirror mirrored the *shed* half but not the *growth* half, pinning its
  target at the 20 ms base forever. On Wi-Fi that bunches arrivals, 20 ms is regularly shorter than
  one delivery stall, so the ring re-primed through every stall for the whole session. Now the full
  `note_read` mirror: 3 underruns in a 5 s window grow the target 10 ms (capped at CoreAudio's 70),
  30 s of quiet steps back, and the write-side hard trim follows the grown target.

### Clients

- **Nothing in the desktop console had ever been clickable.** `SkiaOverlay::handle_event` matched
  only `KeyDown` and `TextInput`, so every mouse button, wheel and touch contact fell past the console
  into the run loop, which routes pointer input exclusively at `stream.capture` — `None` while
  browsing. New `Overlay::handle_pointer` carries mouse/touch in swapchain pixels; the run loop
  converts (it owns the window and hence display scale); the console hit-tests the rects it drew last
  frame. Only **direct** touch devices are offered — an indirect trackpad already drives the mouse.
  Widgets act on **press**, not release, because both carousels scroll the focused item toward centre
  and what you pressed would slide out from under your finger. Host menu on Up from a saved tile;
  `UpdateHost` edits **in place** (remove-and-re-add would silently drop the fingerprint, learned MAC,
  pinned cards and profile binding), and `ForgetHost` arms on first press and fires on second.
- **Discovery went permanently deaf three ways**, each needing an app relaunch: a failed resolve was
  never retried (`browseResultsChangedHandler` fires only when the result *set* changes, and a host
  whose resolve failed is still in the set); a stuck resolve never ended (`NWConnection` has no
  timeout, so the throwaway UDP flow could sit in `.preparing` forever, and a service with a
  connection in flight was skipped); and an `NWBrowser` parking in `.waiting` was ignored — **which is
  exactly where iOS's local-network privacy prompt lands on first launch, and granting it does not
  revive the browser that was already waiting.** A 1 Hz sweep now times out stuck resolves, retries
  failed ones on a 1→30 s backoff, and re-arms a dead browser; the advert's TXT is re-read on every
  browse report. `discovery::Rescan` forces a fresh mdns-sd query — the browse otherwise re-queries on
  a doubling backoff **capped at one hour**, so a long-lived browse is effectively passive. ⚠
  `clients/windows/src/discovery.rs` is a **second copy** of the browse that the earlier IPv4 pinning
  missed; it took an arbitrary first address, so a host whose OS responder answered AAAA rendered a
  card that failed on every click.
- **Phone gyro mirror**, off by default, player 1 / wire pad 0 only, and only while that pad has no
  motion source of its own. iOS/iPadOS only on Apple (`DeviceGyro` wraps `CMDeviceMotion` at ~100 Hz
  on a dedicated serial queue — the controller path's main-queue delivery is a known jitter source);
  Android phones with a gyroscope at ~200 Hz with `maxReportLatencyUs = 0`, since batching is poison
  for gyro aim. Both rotate from the device's natural frame into the controller frame by interface
  orientation, and both send **one zero-gyro sample on stand-down** — the host holds motion as state
  and re-emits it, so a leftover nonzero angular velocity reads as endless rotation.
- **Safe-area resolution** is purely a *sizing* change — no layout change, no input change; pointer
  mapping follows for free since both clients derive the picture rect from the live host mode. Full
  native height, width less left+right safe insets. Portrait settings screens report the housing on
  `top` with zero horizontal insets, so the portrait top inset stands in (gated so an iPad's status
  bar never fabricates one). Android adds the rounded-corner radius, which it does not count as
  cutout. Both even-floor and clamp, because `validate_dimensions` rejects odd dimensions and an inset
  subtraction lands odd about half the time.
- **Gamepad UI**: six sections (Stream · Video · Audio · Controller · Interface · Profiles, plus Input
  on the desktop console) walked with L1/R1 with per-section cursor memory; 12 palettes under one
  shared `ui_palette` key, Violet keeping its explicit sixteen colours so existing installs are an
  identity transform. Presentation only → **device preference, never part of a profile**. Palette
  maths ported three times (Rust/Swift/Kotlin) with the same assertions pinned in each language;
  `every_palette_is_multi_tone` fails under 45° hue spread and caught Ember at 35° and Graphite at 3°.
  Three render-only findings: additive blending blows out over a pale ground, a white scrim at the
  dark field's strength bleaches the gradient, and white glass over a bright field needs more body.

### Session and game lifetime

- **`PunktfunkEndReason` replaces a single "closed" bit** (ABI 17, additive, wire untouched). Five
  values — local, game exited, host ended, host error, lost — classified by the connection watcher
  from close codes already on the wire (`APP_EXITED_CLOSE_CODE` had been sent for a long time with
  nothing consuming it). **Latched before the shutdown flag**, because the two are read by different
  threads and the reason must never arrive second. Exposed as `punktfunk_connection_end_reason` +
  `is_normal()`. Shells fall back to the old wording when there is no verdict (older core, or a close
  that raced the read).
- **The Steam `Running` registry hint was an unbounded veto.** Honouring it reset the absence window
  every pass, so a flag Steam left set — Steam crashed, was closed first, the game re-parented —
  pinned a lease in `running` for the life of the host process. The absence timer now runs
  regardless; past `VETO_LIMIT` (30 s) with nothing of the game on the box, the session ends anyway
  and logs at WARN. Extracted as a pure `exit_confirmed(gone_for, hint_running)` with tests — the
  watch loop polls a live process table and cannot be unit-tested, which is exactly how the
  unbounded veto shipped.
- **New `launchreg.rs`: one record per `(client fingerprint, library id)`**, written at launch and
  independent of the termination policy. The old fingerprint-keyed reclaim only ran under
  `GameOnSessionEnd::Always`, so under the default `Keep` nothing was recorded — and a client retry
  re-sent `Hello::launch` verbatim, which the host obeyed unconditionally. Steam/Epic URIs hid it
  (the launcher just focuses the running copy) but a `gog:`/`custom:` target genuinely started a
  second instance over the same save files. The same retry also minted a fresh `launch_stamp`, so
  procscan refused to adopt a game older than 2 s and **a reconnected session lost game-exit
  detection for the rest of its life.** Identity now flows backwards from the watcher, which
  publishes the concrete `ProcRef`s it adopted; liveness is `Scanner::alive` over that recorded set,
  re-verified by `(pid, start)`. Tradeoffs: a `custom:` command with no detection hints stays
  `Unknown` forever (trading exit detection for not double-spawning), and `IN_FLIGHT_WINDOW` is a
  fixed 90 s, deliberately not `disconnect_grace_seconds`.
- **A launcher entry is `LeaseKind::Untracked` unconditionally**, checked ahead of
  `nested`/`child`/`spec`. Its lifetime previously depended on invisible state: launcher not running
  → live child → `Child` lease → quitting the launcher ended the session; launcher already running →
  command forwards and exits inside `SHIM_WINDOW` → `Untracked` → session persists. Steam Big Picture
  is a *mode*, not a process (and on a Deck it is always running); Heroic is single-instance
  Electron. The real trap was the GameStream path, whose `GsApp` intermediate silently dropped the
  field.

### Library and plugins

- **Store claims keep identity across the scanner-to-plugin handover.** `library.json` gains a v2
  `{entries, claims}` shape that reads the old bare array unchanged and rewrites on first mutation.
  `PUT /library/provider/{p}?store=<s>` claims a store; entries then surface as
  `<store>:<external_id>` rather than `custom:<id>`, so entry ids, GameStream app ids, client art
  caches and Moonlight pins all survive. One provider per store (409 otherwise); while a claim is
  held the matching built-in scanner is skipped, so the two never double-list.
- `GET/PUT /library/scanners` is now a **sources** endpoint over the same disabled-set file.
- New entry fields: `role: game|launcher`; launch kinds `steam_ui` (`bigpicture|desktop`),
  `launcher_ui` (platform-gated, 400 on invalid) and `xbox`.
- **Plugin kit 0.3.0** adds a `./library` subpath: `defineLibraryPlugin` plus ported total parsers —
  text VDF/ACF, the binary `shortcuts.vdf` walker with CRC-32 appid derivation, read-only immutable
  SQLite, a registry wrapper that refuses HKCU, path-confinement joins. `GET/PUT /__config` returns
  `{schema, value}` and persists raw, so a plugin with settings need not ship an SPA.

### Platform and packaging

- **The client's config writer** falls back to an in-place write when the atomic replace is
  unavailable, verifies it by reading the bytes back, and records the last persistence failure
  centrally so the UI can surface it. Scratch files are now per-process, closing a real collision
  between the five processes that write these stores (shell, session, console UI, CLI, Decky) — one
  could previously rename its half-written temp over another's target.
- **Host send pacing** gained a pure, unit-tested budget function: oversized frames are budgeted at
  the pacing rate with a 100 ms absolute ceiling rather than compressed into one frame interval.
  Steady-state schedules are byte-identical, the legacy behaviour stays reachable via an environment
  escape hatch, and the GameStream-compatible path is untouched.
- **Mid-session shard renegotiation is gated off for PyroWave sessions**, which parse the video
  stream in windows fixed at session start — re-sizing mid-stream would corrupt the parse. Those
  sessions get the next-session clamp only and are excluded from jumbo. The ABR decode-cap latch
  likewise does not apply to PyroWave, where adaptive bitrate is open-loop by design.
- **The Deck's Vulkan compatibility layer is built from source**, pinned to the same upstream
  revision as the host's own packaged build — bump both together. ~4 MB of app content replaces a
  94 MB external extension, and Flathub is no longer needed at install time. ⚠ `subprojects/vkroots`
  is a gamescope **submodule** and flatpak-builder clones submodules by default; declaring it again
  as an explicit source breaks the build during extraction. `glm` and `stb` are `.wrap` files, not
  submodules, and *do* need explicit sources.
- **Build-container images push to an authenticated registry endpoint**, and `:latest` is reconciled
  against the content key on every push to main — an out-of-band tag move is detected and repaired
  rather than silently inherited.
- **Windows pad drivers** publish their sequence counters with release ordering (the host was already
  loading with acquire and pairing with nothing) and serialize the output-ring publish. The
  `/dev/uhid` event ABI, previously transcribed into all five Linux gamepad backends, is consolidated
  into one module.

### Verification status

Honest about what has and has not been on hardware, because several things in this release have not:

- **Controller audio has never run on a real DualSense.** Its entire verification is unit tests and
  compile checks, and its rumble arbitration rests on an explicitly retracted assumption about
  whether the voice coils and the rumble motors are the same actuators. The evidence-based 500 ms
  idle window is correct either way, but the underlying exclusivity is unsettled. Android's arbiter
  is the evidence-based one; the desktop twin and the coil restore on Android's stop path are owed.
  Some Android OEM kernels refuse the isochronous claim outright, which degrades to ordinary rumble.
- The **plugin-UI origin split** is validated against a fake console and a fake plugin, not yet in a
  real browser.
- The **packaging default-on changes** have had no installer run or package build.
- **No launcher tile has been clicked on a real host** — the first source that would publish one does
  not exist yet.
- Desktop-audio, packet-sizing and iPad-pointer work is build-verified only.
- ⚠ **The FFmpeg-deletion milestone itself has never executed on a GPU.** It was gated on
  cross-clippy, 160 tests, a workspace check and an `ffmpeg` count of 0 in the client / 2 in the host.
  The software on-glass check, the D3D11 and VAAPI AV1 hardware legs and the field bake were all owed
  at merge; later commits closed some of that but not all. The "no FFmpeg" claim is verified by
  `cargo tree` and a notices-generator mention count, not by inspecting a shipped binary.
- ⚠ **`pf-vaadec` has never decoded a frame anywhere** — no VAAPI hardware was reachable. It is the
  *first* rung on Linux/Intel and unknown vendors; the evidence filter bars it there in favour of
  `pf-vkdecode`, but an explicit pin reaches it.
- **openh264 has never run on glass**; the H.264 software rung is unit-tested only.
- **`native-d3d11va` AV1 is deliberately `verified = false`** — one 25 s 4K60 session, no parity.
- **Split arbitration is opt-in and Linux-wired only**; the Windows arm is built and unit-tested but
  not on hardware. The 5120×1440@240 Main10 behaviour flip is explicitly unvalidated and is named as
  the first thing to re-measure.
- **Software throughput is unmeasured in general** — the CPU rung does 35–39 fps at 4K AV1 against a
  60 fps stream, which is why the backlog flush that triggered the rav1d abort happens at all.
- **The Apple mic fix is a proven root cause, not a verified session.** Its own commits call it "a
  strong inference plus one proven logic defect rather than a confirmed fix" and close "awaiting the
  reporter's on-device confirmation" — which nothing later in the range records. It also leaves a
  known gap: nothing reports whether the uplink actually opened, so the HUD still offers a Mute
  Microphone button over a session that may be sending nothing.
- **The Windows audio substrate is, by contrast, well-evidenced on hardware** — repeated "measured on
  the target box", a live bisect on a fresh endpoint, and a `micpitch` proof reading 440 Hz in →
  440 Hz out at exact peak. The one thing not evidenced is a real client speaking through the minted
  microphone end to end; the pitch proof is probe-driven.
- **The phone-gyro mirror is not recorded as hardware-verified** — remap matrices are pinned by unit
  tests in both languages, but there is no "played a game with a clip-on pad" evidence in the tree.
- **The iOS gamepad-UI pale-palette sweep on glass is still owed**, per its own commit.
- ⚠ **The CI runner scripts are hand-installed** (`/usr/local/bin/ci-docker-prune.sh`,
  `/usr/local/sbin/ci-docker-reclaim.sh`). Merging does not deploy them — both runner hosts need the
  files copied out of `scripts/ci/`, and the missing `192.168.1.58:5011` insecure-registry entry on
  one host is routed around, not fixed.
