//! # punktfunk-core
//!
//! The shared protocol / transport / FEC core for the punktfunk low-latency streaming
//! stack. It is compiled exactly once and linked by every host and client — directly
//! as a Rust `lib`, or across the [C ABI](crate::abi) by Swift / Kotlin / C clients.
//!
//! Everything platform-specific (capture, encode, decode, present, input injection)
//! lives *outside* this crate. What lives *here*:
//!
//! - [`fec`] — erasure coding. GF(2⁸) for GameStream/Moonlight compatibility (P1) and
//!   GF(2¹⁶) Leopard-RS (P2) which removes the ~1 Gbps per-frame shard-count ceiling.
//! - [`packet`] — `#[repr(C)]` zero-copy wire framing: splitting an access unit into
//!   FEC blocks of MTU-sized shards and reassembling them on the far side.
//! - [`crypto`] — AES-128-GCM session sealing, matching GameStream in P1.
//! - [`session`] — the host (submit frame → FEC → packetize → seal → send) and client
//!   (recv → open → reorder → FEC recover → reassemble) state machines.
//! - [`transport`] — pluggable packet I/O (in-process loopback for tests; UDP for real).
//! - [`abi`] — the `extern "C"` surface and `cbindgen`-generated `punktfunk_core.h`.
//! - [`config`] / [`error`] / [`stats`] — session configuration, the shared error/status
//!   vocabulary, and the counters snapshot.
//! - [`input`] — the wire input-event vocabulary (keyboard/mouse/touch, gamepad snapshots).
//! - [`reject`] — typed application-close rejection codes · [`reanchor`] — the post-loss
//!   freeze-until-reanchor client gate · [`render_scale`] — the shared render-scale setting ·
//!   [`audio`] — Opus PCM decode for C-ABI embedders · [`wol`] — Wake-on-LAN.
//! - `quic` (feature `quic`) — the punktfunk/1 control plane: handshake, typed control
//!   messages, pairing (SPAKE2), the datagram plane codecs, and clock sync. With it come
//!   `client` (the embeddable NativeClient worker), `abr` (the adaptive-bitrate
//!   controller), and `clipboard` (the shared-clipboard transport task). `tls`
//!   (feature `tls`) — the pinned-fingerprint certificate verifier.
//!
//! ## Threading contract
//!
//! Nothing in the per-frame path touches an async runtime. `tokio`/`quinn` are gated
//! behind the off-by-default `quic` feature and used only for the control plane.

// Unsafe-proof program: every `unsafe {}` / `unsafe impl` in this crate carries a `// SAFETY:`
// proof. The bulk lives in `abi.rs`, whose sites are instances of the ABI contract stated once at
// the top of that file rather than 141 independent arguments.
//
// Beyond the proofs: `unsafe` is DENIED crate-wide, making the census result a compiler-enforced
// invariant (rust-safety programme): **everything that PARSES network bytes is safe Rust** —
// quic, packet, session, fec, crypto, tls, and the rest. The documented `#![allow(unsafe_code)]`
// carve-outs are exactly two classes, and neither interprets attacker bytes:
//   * the CLIENT surface — `abi` (the `extern "C"` boundary itself) and `client` (embedder glue);
//   * the platform syscall-batching shims under `transport` (`udp/{apple,linux,windows}`,
//     `qos_windows`) — sendmmsg/recvmsg_x/USO/qWAVE move caller-owned buffers, nothing more.
// A new module parsing wire data may NOT add a carve-out.
#![deny(unsafe_code)]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod abi;
#[cfg(feature = "quic")]
mod abr;
pub mod audio;
#[cfg(feature = "quic")]
pub mod client;
/// Client-side shared-clipboard transport: the per-session task that runs the fetch-stream accept
/// loop, drives outbound fetches, and serves inbound ones — surfaced to the embedder as poll
/// events. Wire codecs live in [`quic`]; the OS pasteboard integration lives in the native client.
#[cfg(feature = "quic")]
pub mod clipboard;
pub mod config;
pub mod crypto;
pub mod error;
pub mod fec;
pub mod input;
pub mod packet;
pub mod phase;
#[cfg(feature = "quic")]
pub mod quic;
pub mod reanchor;
pub mod reject;
pub mod render_scale;
pub mod session;
pub mod stats;
#[cfg(feature = "tls")]
pub mod tls;
pub mod transport;
pub mod wol;

pub use config::{CompositorPref, Config, FecConfig, FecScheme, Mode, ProtocolPhase, Role};
pub use error::{PunktfunkError, PunktfunkStatus, Result};
pub use session::{Frame, Session};
pub use stats::Stats;

/// Bump on any breaking change to the [C ABI](crate::abi). Mirrors
/// `punktfunk_abi_version()` and is checked by clients before use.
///
/// v2: `punktfunk_connect` gained `client_cert_pem`/`client_key_pem` (pairing identities);
/// added `punktfunk_pair` / `punktfunk_generate_identity` / `punktfunk_connection_request_mode`.
/// v3: added `punktfunk_wake_on_lan` (Wake-on-LAN magic packet; the host's wake MAC(s) reach
/// clients out-of-band via the mDNS `mac` TXT record, so no connection is required to wake).
/// v4: added `punktfunk_probe` (bounded, trust-agnostic, mDNS-independent reachability handshake —
/// the display-side companion to dial-first, so saved-host "online" pips reflect real reachability).
/// v5: added `punktfunk_connection_next_rumble2` (rumble pull that also yields the self-terminating
/// TTL of a v2 envelope; `punktfunk_connection_next_rumble` is unchanged and drops it). Additive —
/// the wire is backward-compatible (the envelope is a length-tolerant tail on 0xCA), so
/// [`WIRE_VERSION`] is unchanged.
/// v6: added the `punktfunk_reanchor_gate_*` surface (post-loss freeze-until-reanchor gate for the
/// Swift client; Rust embedders use [`reanchor::ReanchorGate`] directly). Additive, client-local —
/// no wire change, so [`WIRE_VERSION`] is unchanged.
/// v7: added `punktfunk_connect_ex8` (`status_out` — typed connect-failure reporting, including
/// the host-rejection block `PUNKTFUNK_STATUS_REJECTED_*` decoded from the host's QUIC
/// application close) and the `PunktfunkStatus` −20 block itself. Additive — the close codes are
/// new application-close vocabulary an old peer simply never sends/reads, so [`WIRE_VERSION`] is
/// unchanged.
/// v8: added the shared-clipboard client surface — `punktfunk_connection_host_caps` and
/// `punktfunk_connection_clipboard_{control,offer,fetch,serve,cancel}` +
/// `punktfunk_connection_next_clipboard`. Additive; the wire grows only backward-compatible control
/// messages (0x40-0x44) and a new `Welcome::host_caps` bit, so [`WIRE_VERSION`] is unchanged.
/// v9: `PunktfunkFrame` grew `received_ns` — the reassembly-completion receipt stamp, so
/// embedders stop stamping receipt at the hand-off pull (which folds the pre-decode queue wait
/// into apparent network latency). Struct-size change on the frame poll surface = a hard ABI
/// break for embedders reading `PunktfunkFrame`; nothing on the wire moved, so [`WIRE_VERSION`]
/// is unchanged.
/// v10: added `punktfunk_connection_clock_offset_now_ns` — the LIVE (mid-stream re-synced)
/// clock offset ongoing latency math must use; the connect-time getter stays frozen by
/// contract. Additive, client-local — no wire change, so [`WIRE_VERSION`] is unchanged.
/// v11: added `punktfunk_connect_ex9` — `connect_ex8` plus a `client_caps` bitfield
/// (`PUNKTFUNK_CLIENT_CAP_CURSOR`, later `…_PHASE_LOCK`), which is how a client tells the host it
/// renders the pointer itself. Additive; the caps ride the existing Hello, so [`WIRE_VERSION`] is
/// unchanged. (Documented late — the bump shipped without its line here.)
/// v12: added `punktfunk_connection_set_cursor_render` — the mid-stream cursor-render flip
/// (design/remote-desktop-sweep.md §8): the client's mouse-model chord tells the host who
/// renders the pointer. Additive; rides the existing control stream (a new message TYPE, which
/// pre-§8 hosts ignore), so [`WIRE_VERSION`] is unchanged.
/// v13: added `punktfunk_connection_send_pen` — the stylus wire plane
/// (design/pen-tablet-input.md): a client sends `RICH_PEN` sample batches once the host
/// advertises `HOST_CAP_PEN`. Additive and capability-gated, so [`WIRE_VERSION`] is unchanged.
/// v14: added `punktfunk_connection_report_phase` + the `PUNKTFUNK_CLIENT_CAP_PHASE_LOCK` mirror
/// — the phase-locked capture reporter (design/phase-locked-capture.md): a client that advertises
/// the cap reports its next display latch (already converted to host clock), the panel period, an
/// uncertainty and the circular arrival-lead statistic the host's controller steers on. Additive;
/// the wire grows only a new control message (`PhaseReport`, 0x32) an old host never reads and a
/// strict-prefix append on the 0xCF host-timing tail, so [`WIRE_VERSION`] is unchanged.
/// v15: versions the shared rumble policy engine's C surface —
/// `punktfunk_connection_next_rumble_cmd`, `punktfunk_connection_set_rumble_quirks` and the
/// `PUNKTFUNK_RUMBLE_QUIRK_*` bits. These symbols are NOT new: they landed while this constant
/// still read 7 and no bump was made, so every core since has exported them while advertising a
/// version that never promised them. That cannot be corrected retroactively — a shipped binary
/// says what it says — so v15 is the floor that *guarantees* them: at or above it the surface is
/// present, below it an embedder must probe for the symbol. Purely a version statement; no code
/// changed with this bump, and no wire change, so [`WIRE_VERSION`] is unchanged.
/// v16: added the pad-audio client surface — `punktfunk_connection_next_pad_audio` (the 0xD1
/// per-gamepad DualSense haptics/speaker plane) + `punktfunk_connection_set_pad_audio_caps` and
/// the `PUNKTFUNK_CLIENT_CAP_PAD_AUDIO` / `PUNKTFUNK_HOST_CAP_PAD_AUDIO` mirrors. Additive and
/// capability-gated end to end: the wire grows a new datagram tag (0xD1) an old client never
/// receives (double-gated caps), a new 0xCD kind (0x06, dropped as unknown by old clients) and
/// arrival flag bits 8/9 sent only toward a capable host, so [`WIRE_VERSION`] is unchanged.
/// v17: added `punktfunk_connection_end_reason` + the `PUNKTFUNK_END_REASON_*` vocabulary — asks,
/// once a session has ended, WHY: this client closed it, the host's launched game exited (its close
/// carried [`quic::APP_EXITED_CLOSE_CODE`], which the host has sent since long before this bump
/// with nothing consuming it), the host ended it cleanly, the host reported a failure, or the
/// connection was simply lost. Purely a read of state the core already had: no new call is required
/// of an embedder, a client that never calls it is unchanged, and the host sends exactly the same
/// bytes either way, so [`WIRE_VERSION`] is unchanged.
/// v18: added `punktfunk_connection_next_rumble_cmd2` — the policy engine's rumble command with the
/// two Xbox impulse-trigger motor levels off the 0xCA v3 tail
/// (`design/trigger-rumble-plane.md`), which the fixed out-params of
/// `punktfunk_connection_next_rumble_cmd` have no room for. A NEW symbol, not a widened one: an
/// exported parameter list is part of the contract, and growing one in place breaks every
/// out-of-tree embedder at once. The old entry point is unchanged in signature AND in the levels
/// it reports — it keeps writing the two handle motors, which is the correct instruction for the
/// actuators it owns, so an embedder that never adopts the new symbol behaves exactly as before.
/// Additive and client-local: the v3 tail has been on the wire (and length-tolerant in both
/// decoders) since it landed, and the host sends the same bytes either way, so [`WIRE_VERSION`] is
/// unchanged.
/// v19: added `punktfunk_connection_note_frame_index_ex` and
/// `punktfunk_reanchor_gate_arm_expecting_drops` — the width-carrying half of the reanchor gate.
/// `note_frame_index_ex` reports how MANY frames an arrival revealed as missing where
/// `punktfunk_connection_note_frame_index` reports only whether any were; passing that width to
/// `arm_expecting_drops` pre-credits the reassembler's `frames_dropped` climb that the same loss
/// produces up to ~120 ms later, so the gate does not read one loss as two and re-freeze a stream a
/// fast LTR-RFI anchor has already healed. NEW symbols, not widened ones — the same rule v18 states:
/// both originals keep their signatures and their behaviour, so an embedder that never adopts either
/// is unchanged (it simply keeps the double-arm race the pair exists to close). Additive and
/// client-local: nothing new goes on the wire — the width is computed from frame indices the client
/// already receives — so [`WIRE_VERSION`] is unchanged.
/// v20: `punktfunk_connection_mgmt_port` — reads the host's management-API port out of the
/// session's `Welcome`, so a client can find the game library WITHOUT mDNS. The port previously
/// existed only in the host's mDNS TXT, which made a host that had moved it off 47990 (the
/// supported way to share a machine with a Sunshine fork, whose web UI owns that port) reachable
/// only where multicast worked — over a VPN, a routed subnet, or for a host added by IP, the
/// library silently fell back to a port nothing was listening on. A NEW symbol, not a widened one:
/// every existing function keeps its signature and behaviour, and an embedder that never calls it
/// is unchanged. The `Welcome` grew a trailing field, which older peers skip in both directions
/// (see `Welcome::encode`), so [`WIRE_VERSION`] is unchanged.
/// v21: added `punktfunk_connect_ex10` — `connect_ex9` plus `device_name`, the label an unpaired
/// client knocks with: what the host's pending-approval list (and the web console's
/// outstanding-pairings view and approve dialog) shows, and the trust-store name on approval. The
/// C ABI had no such parameter, so every embedder took [`client::device_name`]'s OS default —
/// which resolves through `COMPUTERNAME`/`HOSTNAME`, neither of which exists in an Apple GUI
/// process, leaving every Mac, iPad, iPhone and Apple TV knocking as the literal "This device"
/// (a console with three of them pending showed three identical rows). A NEW symbol, not a
/// widened one: `ex9` keeps its parameter list AND its behaviour — it passes a null name, which
/// selects that same default. Additive and client-local: the name rides the `Hello::name` field
/// hosts have read since the pending list existed, so [`WIRE_VERSION`] is unchanged.
/// v22: the per-client access surface (`design/per-client-access.md` §7) —
/// `punktfunk_connection_grants` and `punktfunk_connection_access_expires_in` read the session's
/// LIVE access state (the `PUNKTFUNK_GRANT_*` mask and the countdown to its expiry — Welcome
/// snapshot first, then latest-wins over every mid-session `AccessUpdate` the control task
/// folds in), and `punktfunk_connection_end_reject` reports the typed rejection a mid-session
/// close carried (`PUNKTFUNK_STATUS_REJECTED_*`; `0` = none), because `end_reason` can only
/// file an access-expiry close under HOST_ERROR and that is the wrong sentence for "your
/// access expired". NEW symbols, not widened ones — the same rule v18 states: every existing
/// function keeps its signature and behaviour, and an embedder that never adopts any of the
/// three is unchanged (it simply lacks the courtesy UX; the HOST enforces the grants either
/// way). Additive and client-local: the mask, the expiry and the `AccessUpdate` message all
/// shipped with the Welcome's trailing-field append (old peers skip them in both directions),
/// so [`WIRE_VERSION`] is unchanged.
/// v23: added `punktfunk_connection_audio_plc` — one frame of libopus packet-loss concealment,
/// synthesized from the connection's OWN decoder state, for an embedder whose playout ring is
/// draining because nothing is arriving (design/host-source-stutter-fixes.md WP-C1). The three
/// Rust clients conceal a packet drought on their decode thread; Apple's ring is Swift and its
/// decoder sits behind this ABI, so without a call it had no way to reach the one thing that can
/// extrapolate the missing audio — a second decoder would conceal from empty state, because PLC
/// extrapolates from the last decoded frame. A NEW symbol: every existing function keeps its
/// signature and behaviour, and an embedder that never calls it behaves exactly as before (it
/// simply de-primes over droughts, as all four clients used to). Frames it returns carry `seq`
/// and `pts_ns` of `0` — concealed audio was never on the wire and must not reach an A/V-sync
/// observation. Additive and client-local: nothing new is sent or parsed, so [`WIRE_VERSION`] is
/// unchanged.
/// v24: the lossless audio plane's client surface (`design/hi-res-audio.md` §7) —
/// `punktfunk_connect_ex11` asks for a sample rate and depth (whatever
/// `audio::pcm::rate_is_supported` admits — 48/96 kHz plus the 44.1 kHz family — and 16/24-bit;
/// the accepted rates grew after v24 shipped, which is not an ABI change: no symbol, signature or
/// struct moved, an older header stays correct, and a host that cannot carry a rate declines it to
/// Opus exactly as it always has. Anything but
/// the legacy pair also sets `CLIENT_CAP_AUDIO_HIRES`), and `punktfunk_connection_audio_sample_rate`
/// / `punktfunk_connection_audio_bits` report what the host actually RESOLVED — which may be
/// lower, because the host runs a five-condition gate and every decline lands back on Opus at
/// 48 kHz. `punktfunk_connection_next_audio_pcm` decodes both planes behind the same call, using
/// `pcm::PcmConceal` for gaps on the lossless one (libopus PLC extrapolates from a decoder's
/// model of the signal, and a raw frame has none).
///
/// ADDED, not widened, and this time the distinction has teeth: the natural place for a rate is a
/// field on `PunktfunkAudioPcm`, which is `#[repr(C)]` with no `struct_size` guard and is
/// allocated BY VALUE by every C embedder — growing it would change its layout under all of them
/// at once. `PunktfunkStats` is in the same position. So the format is read through accessors, the
/// same rule v18 set with `next_rumble_cmd2`, and `PUNKTFUNK_AUDIO_SAMPLE_RATE_HZ` keeps its value
/// and its meaning as the DEFAULT/legacy rate — a ring sized from it stays correct for every
/// session that resolves to Opus, which is every session an ABI-23 embedder can ask for. An
/// embedder that adopts none of this behaves exactly as before.
///
/// Client-local in the C sense but NOT wire-free in the usual one: the `Hello`/`Welcome` fields
/// this reads and writes landed with the plane itself, appended behind the existing trailing-field
/// discipline (old peers skip them in both directions, and a legacy request encodes byte-identical
/// to the pre-hi-res messages), so [`WIRE_VERSION`] is still unchanged.
pub const ABI_VERSION: u32 = 24;

/// The punktfunk/1 **wire** version — what `Hello`/`Welcome` carry and hosts equality-check.
/// Deliberately its own constant: [`ABI_VERSION`] tracks the embeddable **C surface**
/// (functions a client links), which can grow without changing a single wire byte — v3's
/// `punktfunk_wake_on_lan` is client-local, and riding the C-ABI bump onto the wire locked
/// every new client out of every deployed host ("ABI mismatch: client 3 host 2", observed
/// live). Bump this ONLY when the handshake/planes actually change incompatibly.
pub const WIRE_VERSION: u32 = 2;
