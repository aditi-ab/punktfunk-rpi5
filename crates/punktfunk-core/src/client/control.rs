//! `CtrlRequest` (the embedder's control-stream requests) and `Negotiated` (the handshake result).

use crate::config::{CompositorPref, GamepadPref, Mode};
use crate::quic::{ClipControl, ClipOffer, ColorInfo, LossReport, ProbeRequest, RfiRequest};

/// A control-stream request the embedder makes on the open handshake stream: a mode switch or a
/// speed test. One outbound channel carries both so the worker's `select!` has a single writer
/// (two `&mut ctrl_send` borrows across select branches don't compile).
pub(crate) enum CtrlRequest {
    Mode(Mode),
    Probe(ProbeRequest),
    Keyframe,
    /// Reference-frame-invalidation recovery: the client saw a `frame_index` gap and reports the
    /// invalidation range so an RFI-capable host re-references a known-good picture instead of
    /// forcing a full IDR. See [`RfiRequest`].
    Rfi(RfiRequest),
    Loss(LossReport),
    /// Adaptive bitrate: ask the host to re-target its encoder (kbps). Sent by the pump's
    /// [`BitrateController`] when the user's bitrate setting is Automatic.
    SetBitrate(u32),
    /// Start a mid-stream clock re-sync batch now (see [`ClockResync`]). Sent by the pump on
    /// its report tick after the first no-op clock flush — the "the clock stepped under me"
    /// signal; the control task also self-triggers one every [`CLOCK_RESYNC_INTERVAL`].
    ClockResync,
    /// Shared-clipboard enable/disable for this session (`design/clipboard-and-file-transfer.md`
    /// §3.1). Idempotent; carries the file-permission flag.
    ClipControl(ClipControl),
    /// Announce that the local clipboard changed — the lazy format-list offer (bytes cross later on
    /// a fetch stream). Symmetric message; the host may send one too.
    ClipOffer(ClipOffer),
    /// Who renders the pointer (cursor-forward sessions): `true` = client draws locally (the
    /// desktop mouse model — host excludes + forwards), `false` = host composites into the
    /// video (the capture model). Sent on every mouse-model flip; idempotent, latest-wins.
    CursorRender(crate::quic::CursorRenderMode),
    /// The client's display-latch grid, ~1 Hz, host-clock timestamps — feeds the host's
    /// phase-locked capture (design/phase-locked-capture.md). Latest-wins; an old host
    /// ignores the message.
    Phase(crate::quic::PhaseReport),
}

/// What the worker reports to [`NativeClient::connect`] once the handshake lands: the
/// [`Welcome`]-resolved session parameters (mode, backends, encode/colour/audio geometry) plus the
/// host certificate fingerprint and the connect-time clock offset. Mirrored one-to-one onto the
/// public `NativeClient` fields of the same names.
#[derive(Clone, Copy)]
pub(crate) struct Negotiated {
    pub(crate) mode: Mode,
    /// Wire shard payload — the chunk-aligned parse window (plan §4.4).
    pub(crate) shard_payload: u16,
    pub(crate) compositor: CompositorPref,
    pub(crate) gamepad: GamepadPref,
    /// SHA-256 of the certificate the host actually presented (TOFU callers persist this).
    pub(crate) host_fingerprint: [u8; 32],
    /// The encoder bitrate the host actually configured (kbps); `0` = an older host.
    pub(crate) bitrate_kbps: u32,
    /// Host clock minus client clock (ns); `0` = no skew handshake (old host / synced clocks).
    pub(crate) clock_offset_ns: i64,
    /// Min RTT of the connect-time skew handshake (ns); `None` = the host never answered —
    /// mid-stream re-syncs are pointless then and stay off. Seeds the re-sync admission
    /// guard's session-floor baseline ([`ResyncGuard`]).
    pub(crate) clock_rtt_ns: Option<u64>,
    /// Resolved encode bit depth: `8`, or `10` for a Main10 / HDR session.
    pub(crate) bit_depth: u8,
    /// Resolved CICP colour signalling.
    pub(crate) color: ColorInfo,
    /// Resolved chroma subsampling as the HEVC `chroma_format_idc` (1 = 4:2:0, 3 = 4:4:4).
    pub(crate) chroma_format: u8,
    /// Resolved audio channel count (2/6/8) — what the Opus decoders must be built from.
    pub(crate) audio_channels: u8,
    /// The single codec the host will emit (`quic::CODEC_*`).
    pub(crate) codec: u8,
    /// The host capability bitfield ([`crate::quic::Welcome::host_caps`]):
    /// [`crate::quic::HOST_CAP_GAMEPAD_STATE`], [`crate::quic::HOST_CAP_CLIPBOARD`]. Exposed to the
    /// embedder via [`NativeClient::host_caps`] so a native client greys out unsupported toggles.
    pub(crate) host_caps: u8,
}
