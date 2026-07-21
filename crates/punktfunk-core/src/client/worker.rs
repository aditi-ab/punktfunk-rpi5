//! `WorkerArgs` (the pump's constructor payload) and `reject_from_close`.

use super::*;
use crate::clipboard::{ClipCommand, ClipEventCore};
use crate::config::{CompositorPref, GamepadPref, Mode};
use crate::error::Result;
use crate::input::InputEvent;
use crate::quic::{HdrMeta, HidOutput, RichInput};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};
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
    pub(crate) video_codecs: u8,
    pub(crate) preferred_codec: u8,
    pub(crate) display_hdr: Option<HdrMeta>,
    pub(crate) launch: Option<String>,
    pub(crate) pin: Option<[u8; 32]>,
    pub(crate) identity: Option<(String, String)>,
    /// The embedder's connect budget (the same value `connect` bounds `ready_rx` with): the
    /// dial loop re-dials a silent host within it, so a host still resuming from Wake-on-LAN
    /// is caught the moment its network comes back instead of failing on the first attempt.
    pub(crate) connect_timeout: std::time::Duration,
    pub(crate) frames: Arc<FrameChannel>,
    pub(crate) audio_tx: SyncSender<AudioPacket>,
    pub(crate) rumble_tx: SyncSender<RumbleUpdate>,
    /// Feed half of the rumble policy engine — its `Drop` (demux task end) marks the engine
    /// closed, so the command API always observes connection teardown.
    pub(crate) rumble_feed: super::rumble::RumbleFeed,
    pub(crate) hidout_tx: SyncSender<HidOutput>,
    pub(crate) hdr_meta_tx: SyncSender<HdrMeta>,
    pub(crate) host_timing_tx: SyncSender<crate::quic::HostTiming>,
    pub(crate) input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    pub(crate) mic_rx: tokio::sync::mpsc::Receiver<(u32, u64, Vec<u8>)>,
    pub(crate) rich_input_rx: tokio::sync::mpsc::UnboundedReceiver<RichInput>,
    pub(crate) ctrl_rx: tokio::sync::mpsc::Receiver<CtrlRequest>,
    pub(crate) ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    /// Inbound clipboard event plane feed — the control task pushes ClipState/ClipOffer, the
    /// clipboard task pushes fetch data; drained by [`NativeClient::next_clip`].
    pub(crate) clip_event_tx: SyncSender<ClipEventCore>,
    /// Outbound clipboard fetch/serve/cancel commands from the embedder → [`crate::clipboard::run`].
    pub(crate) clip_cmd_rx: tokio::sync::mpsc::UnboundedReceiver<ClipCommand>,
    pub(crate) ready_tx: std::sync::mpsc::Sender<Result<Negotiated>>,
    pub(crate) shutdown: Arc<AtomicBool>,
    /// Deliberate-quit flag (see [`NativeClient::quit`]): the worker closes with the quit code if set.
    pub(crate) quit: Arc<AtomicBool>,
    pub(crate) mode_slot: Arc<std::sync::Mutex<Mode>>,
    pub(crate) probe: Arc<Mutex<ProbeState>>,
    pub(crate) frames_dropped: Arc<AtomicU64>,
    pub(crate) fec_recovered: Arc<AtomicU64>,
    pub(crate) hot_tids: Arc<Mutex<Vec<i32>>>,
    /// The live clock offset (see [`NativeClient::clock_offset`]): the worker seeds it with the
    /// connect-time estimate; the control task's mid-stream re-syncs update it.
    pub(crate) clock_offset: Arc<AtomicI64>,
    /// Decode-stage latency samples from the embedder (see [`NativeClient::decode_lat`]): the pump
    /// drains a window mean into the adaptive-bitrate controller's decode signal.
    pub(crate) decode_lat: Arc<Mutex<DecodeLatAcc>>,
}

/// The worker: QUIC handshake, then the input/datagram/control tasks + the blocking
/// data-plane pump.
/// The host's stated rejection, if this connection was closed with a typed application code
/// (see [`crate::reject`]) — `None` for local errors, bare/legacy closes (including our own
/// `LocallyClosed`), and transport failures, which keep their original error.
pub(crate) fn reject_from_close(conn: &quinn::Connection) -> Option<crate::reject::RejectReason> {
    match conn.close_reason()? {
        quinn::ConnectionError::ApplicationClosed(ac) => u32::try_from(u64::from(ac.error_code))
            .ok()
            .and_then(crate::reject::RejectReason::from_close_code),
        _ => None,
    }
}
