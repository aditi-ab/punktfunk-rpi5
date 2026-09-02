//! Constructor bag the connect path hands the tokio worker, plus typed close-code
//! classification.
//!
//! [`WorkerArgs`] holds Hello fields, event planes, and the live slots the control
//! task mutates. [`reject_from_close`] maps a QUIC application close onto
//! [`crate::reject::RejectReason`]; transport and local closes keep the original error.

use super::*;
use crate::clipboard::{ClipCommand, ClipEventCore};
use crate::config::{CompositorPref, GamepadPref, Mode};
use crate::error::Result;
use crate::input::InputEvent;
use crate::quic::{HdrMeta, HidOutput, PadAudioFrame};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

pub(crate) struct WorkerArgs {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) mode: Mode,
    pub(crate) compositor: CompositorPref,
    pub(crate) gamepad: GamepadPref,
    pub(crate) bitrate_kbps: u32,
    pub(crate) video_caps: u8,
    pub(crate) audio_channels: u8,
    /// Hello request, never the device format. The host answers in `Welcome`; open
    /// the device from that. Anything other than 48 kHz/16-bit also sets
    /// [`crate::quic::CLIENT_CAP_AUDIO_HIRES`].
    pub(crate) audio_rate_hz: u32,
    pub(crate) audio_bits: u8,
    pub(crate) video_codecs: u8,
    pub(crate) preferred_codec: u8,
    pub(crate) display_hdr: Option<HdrMeta>,
    pub(crate) client_caps: u8,
    /// Slice-progressive [`crate::session::Frame::part`] opt-in. Ignored on all-intra
    /// (PyroWave) sessions — newest-wins draining needs whole AUs.
    pub(crate) frame_parts: bool,
    pub(crate) launch: Option<String>,
    /// Display name in `Hello` — the host's approval-list / trust-store label.
    pub(crate) name: Option<String>,
    pub(crate) pin: Option<[u8; 32]>,
    pub(crate) identity: Option<(String, String)>,
    /// Same budget `connect` bounds `ready_rx` with. The dial loop re-dials inside it
    /// so a host still coming up from Wake-on-LAN is not a first-attempt failure.
    pub(crate) connect_timeout: std::time::Duration,
    pub(crate) frames: Arc<FrameChannel>,
    pub(crate) audio_tx: SyncSender<AudioPacket>,
    pub(crate) rumble_tx: SyncSender<RumbleUpdate>,
    /// Feed half of the rumble policy engine. Its `Drop` (demux task end) marks the
    /// engine closed, so the command API always sees teardown.
    pub(crate) rumble_feed: super::rumble::RumbleFeed,
    pub(crate) hidout_tx: SyncSender<HidOutput>,
    /// Inbound `0xD1` pad-audio frames (voice-coil haptics + speaker).
    pub(crate) pad_audio_tx: SyncSender<PadAudioFrame>,
    /// Per-pad render caps (bit0 haptics, bit1 speaker). OR'd into GamepadArrival
    /// flags (bits 8/9) toward a `HOST_CAP_PAD_AUDIO` host only.
    pub(crate) pad_audio_caps: Arc<[AtomicU8; crate::input::MAX_PADS]>,
    pub(crate) hdr_meta_tx: SyncSender<HdrMeta>,
    pub(crate) host_timing_tx: SyncSender<crate::quic::HostTiming>,
    pub(crate) cursor_shape_tx: SyncSender<crate::quic::CursorShape>,
    pub(crate) cursor_state_tx: SyncSender<crate::quic::CursorState>,
    pub(crate) input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    pub(crate) mic_rx: tokio::sync::mpsc::Receiver<(u32, u64, Vec<u8>)>,
    /// Pre-encoded `0xCC` datagrams — rich input and pen batches share this queue.
    pub(crate) rich_input_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    pub(crate) ctrl_rx: tokio::sync::mpsc::Receiver<CtrlRequest>,
    pub(crate) ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    /// Clipboard event plane: control task pushes ClipState/ClipOffer, clipboard
    /// task pushes fetch data.
    pub(crate) clip_event_tx: SyncSender<ClipEventCore>,
    pub(crate) clip_cmd_rx: tokio::sync::mpsc::UnboundedReceiver<ClipCommand>,
    pub(crate) ready_tx: std::sync::mpsc::Sender<Result<Negotiated>>,
    pub(crate) shutdown: Arc<AtomicBool>,
    /// [`crate::client::PunktfunkEndReason`] as `u8`, latched beside `shutdown`.
    pub(crate) end_reason: Arc<AtomicU8>,
    /// When set, the worker closes with the deliberate-quit code rather than a generic end.
    pub(crate) quit: Arc<AtomicBool>,
    pub(crate) mode_slot: Arc<std::sync::Mutex<Mode>>,
    pub(crate) probe: Arc<Mutex<ProbeState>>,
    pub(crate) frames_dropped: Arc<AtomicU64>,
    pub(crate) fec_recovered: Arc<AtomicU64>,
    /// Pump mic task counts wire sends and stale-shed drops; the producer counts
    /// queue-full drops.
    pub(crate) mic_stats: Arc<MicUplinkCounters>,
    pub(crate) hot_tids: Arc<Mutex<Vec<i32>>>,
    /// Seeded with the connect-time estimate; the control task's mid-stream re-syncs
    /// update it.
    pub(crate) clock_offset: Arc<AtomicI64>,
    /// Embedder decode-stage samples. The pump drains a window mean into the ABR
    /// decode signal.
    pub(crate) decode_lat: Arc<Mutex<DecodeLatAcc>>,
    /// Encoder-target mirror. Seeded from Welcome; updated on every `BitrateChanged` ack.
    pub(crate) live_bitrate: Arc<AtomicU32>,
    /// Live grants. Seeded from the Welcome advert; every `AccessUpdate` overwrites
    /// (latest wins).
    pub(crate) access_grants: Arc<AtomicU32>,
    /// Client-wall-clock unix seconds; `0` = permanent. Seeded from Welcome
    /// `expires_in_secs`, re-anchored by every `AccessUpdate`.
    pub(crate) access_deadline_unix: Arc<AtomicU64>,
    /// Pushed by the control task only AFTER it has folded the update into the two
    /// live slots above.
    pub(crate) access_tx: SyncSender<crate::quic::AccessUpdate>,
    /// Typed mid-session close from [`crate::reject::RejectReason`]; `0` = none.
    /// Latched beside `end_reason` so an access-expiry close is not rendered as a
    /// generic host error.
    pub(crate) end_reject_code: Arc<AtomicU32>,
}

/// The host's stated rejection, if the connection closed with a typed application code.
/// `None` for local errors, bare/legacy closes (including our own `LocallyClosed`), and
/// transport failures — those keep their original error.
pub(crate) fn reject_from_close(conn: &quinn::Connection) -> Option<crate::reject::RejectReason> {
    match conn.close_reason()? {
        quinn::ConnectionError::ApplicationClosed(ac) => u32::try_from(u64::from(ac.error_code))
            .ok()
            .and_then(crate::reject::RejectReason::from_close_code),
        _ => None,
    }
}
