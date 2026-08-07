//! Shared audio layout: the single source of truth for Opus (multi)stream surround across the
//! host, the GameStream compatibility path, and every client decoder.
//!
//! **Canonical wire channel order** is `FL FR FC LFE RL RR SL SR` (the GameStream/Moonlight
//! order, and the PipeWire/PulseAudio default map for 6/8 channels). Every host capturer
//! delivers PCM in this order and every client decodes into it, so the Opus multistream
//! `mapping` is the **identity** (`[0, 1, …, channels-1]`) on both ends — punktfunk owns the
//! encoder and every decoder, so the GFE-style pre-rotation Moonlight needs over SDP
//! (`gamestream::audio::surround_params`) is a GameStream-only concern and never touches the
//! native `punktfunk/1` path.
//!
//! Channel counts the protocol negotiates: `2` (stereo), `6` (5.1) and `8` (7.1). Anything
//! else clamps to stereo ([`normalize_channels`]).

/// Canonical wire channel positions; the index is the channel's slot in the interleaved PCM
/// frame. A count of N uses positions `0..N` (always a prefix of this 8-channel order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WirePos {
    FrontLeft = 0,
    FrontRight = 1,
    FrontCenter = 2,
    Lfe = 3,
    RearLeft = 4,
    RearRight = 5,
    SideLeft = 6,
    SideRight = 7,
}

/// The full 8-channel wire order; the N-channel order is its first N entries.
pub const WIRE_ORDER_8: [WirePos; 8] = {
    use WirePos::*;
    [
        FrontLeft,
        FrontRight,
        FrontCenter,
        Lfe,
        RearLeft,
        RearRight,
        SideLeft,
        SideRight,
    ]
};

/// One Opus (multi)stream layout. `mapping` is the libopus multistream mapping we encode AND
/// decode with — identity, since punktfunk owns both ends. `streams`/`coupled` give the
/// normal-quality coupling (FL,FR)+(FC,LFE) [+(RL,RR) on 7.1] with the remaining channels as
/// mono streams; high quality is one mono stream per channel. Bitrates match Sunshine's
/// per-config values (stereo keeps punktfunk's live-validated 128 kbps).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpusLayout {
    /// Interleaved channel count (2, 6 or 8).
    pub channels: u8,
    /// Number of Opus streams in the multistream packet.
    pub streams: u8,
    /// How many of those streams are coupled (stereo) pairs.
    pub coupled: u8,
    /// libopus multistream channel mapping — identity `[0, 1, …, channels-1]`.
    pub mapping: &'static [u8],
    /// Target Opus bitrate in bits/sec at [`AudioTier::Standard`] — see
    /// [`OpusLayout::bitrate_for`], which is what callers should use. These are the historical
    /// values, kept exactly so `Standard` reproduces the pre-tier wire byte-for-byte.
    ///
    /// The GameStream plane encodes hard-CBR from these (its audio FEC needs a constant packet
    /// size); the native plane uses constrained VBR, where that constraint does not apply.
    pub bitrate: i32,
}

/// Stereo: a plain coupled pair. The 128 kbps live-validated config.
pub const LAYOUT_STEREO: OpusLayout = OpusLayout {
    channels: 2,
    streams: 1,
    coupled: 1,
    mapping: &[0, 1],
    bitrate: 128_000,
};
/// 5.1 normal quality: (FL,FR)+(FC,LFE) coupled, RL+RR mono.
pub const LAYOUT_51: OpusLayout = OpusLayout {
    channels: 6,
    streams: 4,
    coupled: 2,
    mapping: &[0, 1, 2, 3, 4, 5],
    bitrate: 256_000,
};
/// 5.1 high quality: one mono stream per channel.
pub const LAYOUT_51_HQ: OpusLayout = OpusLayout {
    channels: 6,
    streams: 6,
    coupled: 0,
    mapping: &[0, 1, 2, 3, 4, 5],
    bitrate: 1_536_000,
};
/// 7.1 normal quality: (FL,FR)+(FC,LFE)+(RL,RR) coupled, SL+SR mono.
pub const LAYOUT_71: OpusLayout = OpusLayout {
    channels: 8,
    streams: 5,
    coupled: 3,
    mapping: &[0, 1, 2, 3, 4, 5, 6, 7],
    bitrate: 450_000,
};
/// 7.1 high quality: one mono stream per channel.
pub const LAYOUT_71_HQ: OpusLayout = OpusLayout {
    channels: 8,
    streams: 8,
    coupled: 0,
    mapping: &[0, 1, 2, 3, 4, 5, 6, 7],
    bitrate: 2_048_000,
};

/// Encode bitrate tier for the desktop-audio downlink. The layout table's `bitrate` is the
/// [`AudioTier::Standard`] value, so `Standard` reproduces the pre-tier wire byte-for-byte.
///
/// **Why a tier at all.** 5 ms Opus frames are markedly less efficient than 20 ms ones (shorter
/// MDCT, a bigger per-packet overhead share), so the historical 128 kbps stereo buys roughly what
/// ~100 kbps buys at 20 ms — audible on music, and the 2026-08-03 field report said exactly that.
/// Meanwhile the same session carries tens of Mbps of video: at 256 kbps audio is ~1 % of the
/// budget. [`AudioTier::High`] is therefore the DEFAULT; the lower tiers exist for a genuinely
/// constrained link, not as the normal case.
///
/// Purely a host-side encoder knob: every client decodes whatever bitrate arrives (libopus reads
/// it from the packet), so changing tiers needs no protocol negotiation and no client change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AudioTier {
    /// Constrained links — noticeably lossy on music, still fine for game/voice content.
    Low,
    /// The historical values (stereo 128 kbps). Kept exactly so the tier machinery is provably
    /// non-regressive against every pre-tier build.
    Standard,
    /// The default: effectively transparent at 5 ms frames, for ~1 % of a normal video budget.
    #[default]
    High,
}

impl AudioTier {
    /// Parse a config/CLI spelling (`low` / `standard` / `high`); `None` for anything else so the
    /// caller can warn and fall back rather than silently downgrading someone's audio.
    pub fn parse(s: &str) -> Option<AudioTier> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Some(AudioTier::Low),
            "standard" | "normal" | "medium" => Some(AudioTier::Standard),
            "high" => Some(AudioTier::High),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AudioTier::Low => "low",
            AudioTier::Standard => "standard",
            AudioTier::High => "high",
        }
    }
}

impl OpusLayout {
    /// This layout's target bitrate at `tier`. The uncoupled HIGH-QUALITY layouts
    /// ([`LAYOUT_51_HQ`] / [`LAYOUT_71_HQ`]) are already far past transparency, so they are
    /// tier-invariant — scaling 1.5 Mbps up would only waste wire.
    pub fn bitrate_for(&self, tier: AudioTier) -> i32 {
        // One mono stream per channel == the HQ layouts; nothing to gain from a tier there.
        if self.coupled == 0 && self.streams == self.channels {
            return self.bitrate;
        }
        match (self.channels, tier) {
            (6, AudioTier::Low) => 192_000,
            (6, AudioTier::High) => 448_000,
            (8, AudioTier::Low) => 320_000,
            (8, AudioTier::High) => 768_000,
            (_, AudioTier::Low) => 96_000,
            (_, AudioTier::High) => 256_000,
            (_, AudioTier::Standard) => self.bitrate,
        }
    }
}

/// What the audio plane will actually cost this session: the tier to encode at, and whether the
/// redundant `0xD2` plane is affordable. Produced by [`plan_audio_budget`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioBudget {
    pub tier: AudioTier,
    pub redundancy: bool,
    /// Total wire cost in kbps, redundancy included — what the decision was made against.
    pub kbps: u32,
}

/// Share of the session's video bitrate the audio plane may spend. Audio rides QUIC datagrams,
/// OUTSIDE the ABR loop, so whatever it takes is taken off the top and adaptive bitrate can
/// neither see nor reclaim it — which is exactly why it needs a budget of its own.
const AUDIO_BUDGET_PCT: u32 = 5;
/// …but never squeeze audio below the Low tier. A stream with unintelligible audio is worse than
/// one that spends a few percent more, and the floor is what stops a very low video bitrate from
/// silently producing a useless audio plane.
const AUDIO_BUDGET_FLOOR_KBPS: u32 = 96;

/// Choose the encode tier and whether to send redundancy, given the session's resolved VIDEO
/// bitrate.
///
/// **Why this exists.** Tier `High` and the redundant plane were introduced separately, each
/// justified as "about 1 % of the video budget" — but they multiply: 256 kbps stereo sent twice is
/// 512 kbps, which is ~2.5 % of a 20 Mbps session and ~10 % of a 5 Mbps one. Nothing added the two
/// together, and nothing capped the total, so on a constrained link the audio plane quietly took a
/// tenth of the bandwidth that ABR was carefully managing the rest of.
///
/// The ladder is ordered by preference, not by cost: transparent audio beats redundant audio (the
/// complaint this whole program came from was quality, and the redundancy only pays off under
/// loss), so `High` alone outranks `Standard` + redundancy even though they cost the same.
/// `requested` lets an operator ask for a specific tier; the budget can lower it but never raises
/// it above what was asked.
pub fn plan_audio_budget(
    video_kbps: u32,
    channels: u8,
    requested: AudioTier,
    client_wants_redundancy: bool,
) -> AudioBudget {
    let budget = (video_kbps.saturating_mul(AUDIO_BUDGET_PCT) / 100).max(AUDIO_BUDGET_FLOOR_KBPS);
    let layout = layout_for(channels, false);
    let cost = |tier: AudioTier, red: bool| -> u32 {
        let one = (layout.bitrate_for(tier) / 1000).max(0) as u32;
        if red {
            one.saturating_mul(2)
        } else {
            one
        }
    };
    // Preference order, best first. An operator asking for `Low` must not be handed `High`, so
    // candidates above the request are filtered out.
    let rank = |t: AudioTier| match t {
        AudioTier::Low => 0,
        AudioTier::Standard => 1,
        AudioTier::High => 2,
    };
    let ladder = [
        (AudioTier::High, true),
        (AudioTier::High, false),
        (AudioTier::Standard, true),
        (AudioTier::Standard, false),
        (AudioTier::Low, false),
    ];
    for (tier, red) in ladder {
        if rank(tier) > rank(requested) || (red && !client_wants_redundancy) {
            continue;
        }
        let kbps = cost(tier, red);
        if kbps <= budget {
            return AudioBudget {
                tier,
                redundancy: red,
                kbps,
            };
        }
    }
    // Nothing fit — take the cheapest thing that still works rather than muting audio.
    AudioBudget {
        tier: AudioTier::Low,
        redundancy: false,
        kbps: cost(AudioTier::Low, false),
    }
}

/// Pick the layout for a negotiated channel count. Unknown counts fall back to stereo (clients
/// only ever request 2/6/8). `high_quality` selects the uncoupled high-bitrate config.
pub fn layout_for(channels: u8, high_quality: bool) -> &'static OpusLayout {
    match (channels, high_quality) {
        (6, false) => &LAYOUT_51,
        (6, true) => &LAYOUT_51_HQ,
        (8, false) => &LAYOUT_71,
        (8, true) => &LAYOUT_71_HQ,
        _ => &LAYOUT_STEREO,
    }
}

/// Clamp an arbitrary (wire / requested) channel count to one the protocol negotiates. `0`,
/// absent, or any unsupported value becomes stereo.
pub fn normalize_channels(requested: u8) -> u8 {
    match requested {
        6 => 6,
        8 => 8,
        _ => 2,
    }
}

/// Loss detector for the client audio plane, shared by every platform decoder.
///
/// The `0xC9` audio datagrams carry a per-packet sequence the host advances by 1 (wrapping), but
/// ride the lossy datagram plane with no FEC — a lost 5 ms Opus packet used to play out as a hard
/// gap (a click/pop; the jitter rings just emit silence). Feeding this tracker each received
/// packet's sequence tells the decoder how many packets went missing *immediately before it*, so
/// it can synthesize that many frames of libopus packet-loss concealment (`decode` with empty
/// input) before decoding the real one — turning clicks into an inaudible interpolation.
///
/// Reorders and duplicates conceal nothing (the plane has no reorder buffer; playing a late
/// packet where it lands is the existing behaviour), and a gap is capped at
/// [`MAX_CONCEAL_PACKETS`] (50 ms at the protocol's 5 ms frames) — libopus PLC fades to silence
/// after a few frames anyway, so past the cap the ring's underrun/re-prime path takes over as
/// before.
#[derive(Debug, Default)]
pub struct AudioGapTracker {
    /// Sequence of the newest packet seen (`None` until the first).
    last_seq: Option<u32>,
}

/// Most packets a single gap will ask concealment for (50 ms at the protocol's 5 ms frames).
/// Crate-internal: callers only ever see `missing_before`'s already-capped count (and cbindgen
/// must not export it — it's not part of the C ABI). `pub(crate)` for the in-core PCM decoder
/// (`abi.rs`), which sizes its no-realloc output buffer from it.
pub(crate) const MAX_CONCEAL_PACKETS: u32 = 10;

impl AudioGapTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next received packet's sequence; returns how many packets are missing immediately
    /// before it (`0` for in-order, the first packet, duplicates, and reorders), capped at
    /// [`MAX_CONCEAL_PACKETS`]. Wrapping-safe: a sequence in the backward half of the u32 space is
    /// a reorder, not a 2³¹-packet gap.
    pub fn missing_before(&mut self, seq: u32) -> u32 {
        let Some(last) = self.last_seq else {
            self.last_seq = Some(seq);
            return 0;
        };
        let delta = seq.wrapping_sub(last);
        if delta == 0 || delta > u32::MAX / 2 {
            return 0; // duplicate, or a reorder older than the newest — nothing to conceal
        }
        self.last_seq = Some(seq);
        (delta - 1).min(MAX_CONCEAL_PACKETS)
    }
}

/// Rebuilds the audio stream from the redundant `0xD2` plane, so a single lost datagram is
/// RECOVERED rather than concealed.
///
/// Deliberately lives in core, on the demux side, rather than in the four client decoders. The
/// recovered frame is re-inserted into the same queue in order, so every embedder — Linux,
/// Windows, Android, Apple, and any C-ABI consumer — gets a complete stream with no change at all,
/// and their [`AudioGapTracker`] simply stops seeing the gap.
///
/// **Only the immediately-preceding frame can be recovered**, because that is all the wire carries
/// (see [`crate::quic::encode_audio_red_datagram`]). A longer burst still falls through to
/// packet-loss concealment — but it falls through one frame shorter, which is strictly better.
#[derive(Debug, Default)]
pub struct AudioRedRecovery {
    /// Sequence of the newest packet handed downstream.
    last_seq: Option<u32>,
}

impl AudioRedRecovery {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the arriving datagram's sequence and whether it carried a redundant copy. Returns
    /// `true` when that copy should be emitted (as `seq - 1`) BEFORE the packet itself.
    ///
    /// Wrapping-safe, and conservative in both directions: a reorder or duplicate recovers
    /// nothing, and neither does the first packet of a session (nothing is known to be missing).
    pub fn recover_before(&mut self, seq: u32, has_prev: bool) -> bool {
        let recover = match self.last_seq {
            // Nothing emitted yet: no evidence anything was lost, so inserting the predecessor
            // would prepend audio the client never missed.
            None => false,
            Some(last) => {
                let delta = seq.wrapping_sub(last);
                // `delta == 1` is in-order; `delta >= 2` (forward half of the space only) means
                // at least the predecessor is missing.
                has_prev && (2..u32::MAX / 2).contains(&delta)
            }
        };
        self.last_seq = Some(match self.last_seq {
            // A reorder must not drag the anchor backwards.
            Some(last) if seq.wrapping_sub(last) > u32::MAX / 2 => last,
            _ => seq,
        });
        recover
    }
}

// ---- the shared playback de-jitter policy -------------------------------------------------

/// The protocol's audio frame, in milliseconds — every host datagram carries exactly one
/// ([`crate::quic::encode_audio_datagram`]), so it is also the smallest useful shed unit.
pub const FRAME_MS: u32 = 5;

/// Tuning for [`JitterPolicy`], in MILLISECONDS.
///
/// Denominating the depth in time rather than in device quanta is the point. Every client used to
/// compute its target as `3 × quantum`, which is a sane 15 ms at a 5 ms quantum and a silent 64 ms
/// at a 20 ms one — the same source line meaning two very different latencies depending on what
/// else happened to be using the audio graph that day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitterTuning {
    /// Depth to prime to before the first sample plays, and the depth drift correction pulls
    /// back toward. The adaptive floor may raise the live target above this; it never goes below.
    pub base_target_ms: u32,
    /// Ceiling for the adaptively-grown target (see [`JitterPolicy::note_read`]).
    pub max_target_ms: u32,
    /// Slack above the live target before drop-oldest trimming starts. Absorbs an arrival burst
    /// without overflowing.
    ///
    /// Drift correction sheds at the MIDDLE of this band (see [`JitterTuning::shed_excess_ms`]),
    /// so the smooth correction always gets its chance before the hard trim. Setting this too
    /// small is a real failure mode, not just a tuning choice: if the trim point sits below the
    /// shed point, the ring is trimmed back before the depth average can ever reach the shed
    /// threshold, drift correction becomes dead code, and every correction is once again the
    /// audible drop it was supposed to replace.
    pub headroom_ms: u32,
    /// Absolute bound on buffered audio — the only hard guarantee on added latency.
    pub hard_cap_ms: u32,
    /// Consecutive short reads before the ring goes back to priming. `1` reproduces the old
    /// `if ring.is_empty() { primed = false }`, where a single transient drain manufactured a
    /// whole target's worth of fresh silence; every platform now uses hysteresis.
    pub deprime_after: u32,
}

impl JitterTuning {
    /// PipeWire adaptively rate-matches the stream to the graph clock and absorbs a shallow ring,
    /// so Linux can run tight.
    pub const PIPEWIRE: JitterTuning = JitterTuning {
        base_target_ms: 15,
        max_target_ms: 60,
        headroom_ms: 25,
        hard_cap_ms: 80,
        deprime_after: 4,
    };
    /// WASAPI shared-mode event-driven render: the engine buffers for us, but nothing rate-matches.
    pub const WASAPI: JitterTuning = JitterTuning {
        base_target_ms: 20,
        max_target_ms: 70,
        headroom_ms: 30,
        hard_cap_ms: 90,
        deprime_after: 4,
    };
    /// CoreAudio via AVAudioEngine — comparable to WASAPI; the iOS IO buffer is already 5 ms.
    pub const COREAUDIO: JitterTuning = JitterTuning {
        base_target_ms: 20,
        max_target_ms: 70,
        headroom_ms: 30,
        hard_cap_ms: 90,
        deprime_after: 4,
    };
    /// AAudio hands us a raw realtime callback and makes us own the buffer, and Wi-Fi power-save
    /// bunching lands as underruns = crackle. Android therefore starts DEEPER — but at 25 ms, not
    /// the old fixed 40: the adaptive floor raises it only on the devices that actually underrun,
    /// instead of every device pre-paying for the worst one.
    pub const AAUDIO: JitterTuning = JitterTuning {
        base_target_ms: 25,
        max_target_ms: 90,
        headroom_ms: 40,
        hard_cap_ms: 120,
        deprime_after: 5,
    };

    /// How far above the live target the depth average must sit before drift correction sheds:
    /// the middle of the headroom band, but never less than two protocol frames (so it cannot be
    /// hair-triggered by one quantum of normal swing). Deriving it from `headroom_ms` rather than
    /// fixing it absolutely is what keeps the smooth shed strictly BELOW the hard trim on every
    /// preset — see the field on `headroom_ms`.
    pub const fn shed_excess_ms(&self) -> u32 {
        let half = self.headroom_ms / 2;
        if half > 2 * FRAME_MS {
            half
        } else {
            2 * FRAME_MS
        }
    }
}

/// What one callback should do, from [`JitterPolicy::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct JitterStep {
    /// Interleaved samples to discard from the FRONT of the ring before reading.
    pub drop_front: usize,
    /// When non-zero, `drop_front` is a smooth drift correction and this many interleaved samples
    /// of linear crossfade should be applied across the seam ([`crossfade_drop`] does it for a
    /// `VecDeque<f32>` ring). Zero means discard hard — either nothing is being dropped, or the
    /// ring blew the hard cap and is already a discontinuity.
    pub crossfade: usize,
    /// Emit silence this callback: still priming, or re-priming after a sustained drain.
    pub silence: bool,
}

/// EWMA time constant for the depth average, in ms. Long enough that a burst doesn't trigger a
/// shed, short enough to track real drift.
const EWMA_TAU_MS: u32 = 1_000;
/// The depth EWMA must stay above the shed threshold for this much CONSUMED AUDIO. Deliberately long: a shed is the only
/// thing here a listener could ever notice, so it must never fire on a transient.
const SHED_SUSTAIN_MS: u32 = 2_000;
/// Linear crossfade applied across a drift shed's seam.
const SHED_CROSSFADE_MS: u32 = 2;
/// Underruns inside [`GROW_WINDOW_MS`] before the live target grows.
const GROW_UNDERRUNS: u32 = 3;
const GROW_WINDOW_MS: u32 = 5_000;
const GROW_STEP_MS: u32 = 10;
/// Quiet time (no underrun) before a grown target relaxes one step back toward the base.
const SHRINK_QUIET_MS: u32 = 30_000;
/// The same, while the A/V sync loop is actively asking for a shallower ring — see the branch in
/// [`JitterPolicy::note_read`] that selects between them.
const SHRINK_QUIET_SYNC_MS: u32 = 5_000;

/// The playback de-jitter state machine shared by every client's audio ring.
///
/// **The defect it exists to fix.** Every client's ring primed *up* to a target and clamped at a
/// ceiling, and none of them walked the depth back *down*. Any transient — a Wi-Fi arrival burst, a
/// host stall, or plain host-DAC-vs-client-DAC clock skew of a few dozen ppm — therefore added
/// latency permanently, until an underrun happened to re-prime. Android, with no shed at all,
/// converged on its hard cap and stayed there; Apple shed 40 ms at once and its own comment called
/// that "one audible blip". Here, a depth EWMA that sits [`SHED_EXCESS_MS`] above target for
/// [`SHED_SUSTAIN_MS`] of consumed audio sheds ONE 5 ms frame with a crossfade, so latency returns
/// to target instead of ratcheting.
///
/// **Driven by the audio clock, not the wall clock**: every duration is measured in samples
/// consumed. That makes it allocation-free, syscall-free (safe in a realtime callback) and
/// deterministic under test.
#[derive(Clone, Debug)]
pub struct JitterPolicy {
    tuning: JitterTuning,
    /// Interleaved samples per millisecond at the negotiated layout (48 × channels).
    per_ms: usize,
    /// The live target, in interleaved samples — `base_target_ms` grown by underrun pressure.
    target: usize,
    primed: bool,
    /// Consecutive short reads (de-prime hysteresis).
    empties: u32,
    /// EWMA of ring depth, interleaved samples.
    depth_avg: f32,
    /// Consumed samples for which the EWMA has stayed above the shed threshold.
    over_run: usize,
    /// Underruns seen in the current growth window, and the window's consumed-sample count.
    underruns: u32,
    window_run: usize,
    /// Consumed samples since the last underrun (drives the relax-back-down step).
    quiet_run: usize,
    /// `want` from the most recent [`step`](Self::step), so [`note_read`](Self::note_read) can
    /// advance the sample-denominated timers without the caller repeating it.
    last_want: usize,
    /// Depth the A/V sync loop would like, in interleaved samples ([`AvSync::desired_depth`]).
    /// `None` — the default, and what every un-wired ring keeps — reproduces the pre-sync
    /// behaviour exactly, which is what lets the four client rings adopt this one at a time
    /// without diverging in the meantime.
    sync_target: Option<usize>,
}

impl JitterPolicy {
    /// `channels` is the negotiated interleaved channel count (2/6/8).
    pub fn new(tuning: JitterTuning, channels: u8) -> JitterPolicy {
        let per_ms = (SAMPLE_RATE_HZ / 1000) as usize * channels.max(1) as usize;
        JitterPolicy {
            tuning,
            per_ms,
            target: tuning.base_target_ms as usize * per_ms,
            primed: false,
            empties: 0,
            depth_avg: 0.0,
            over_run: 0,
            underruns: 0,
            window_run: 0,
            quiet_run: 0,
            last_want: 0,
            sync_target: None,
        }
    }

    /// Hand the ring the depth the A/V sync loop wants ([`AvSync::desired_depth`]), or `None` to
    /// run unsynchronised.
    ///
    /// This is a REQUEST, not a command. [`effective_target`](Self::effective_target) clamps it
    /// between the underrun-driven adaptive floor and the hard cap, so sync can never starve the
    /// ring: if the link's jitter needs more buffer than the picture is away, the floor wins and
    /// the residual shows up on the HUD instead of as a dropout. That ordering is the whole safety
    /// argument for steering playback depth from a network measurement at all.
    pub fn set_sync_target(&mut self, target: Option<usize>) {
        self.sync_target = target;
    }

    /// The sync loop is asking to run shallower than the adaptive target has grown to.
    fn sync_wants_less(&self) -> bool {
        self.sync_target.is_some_and(|s| s < self.target)
    }

    /// The live target depth in ms (grows under underrun pressure; never below the base).
    pub fn target_ms(&self) -> u32 {
        (self.target / self.per_ms) as u32
    }

    /// Convert a ring depth in interleaved samples to milliseconds — for stats/HUD reporting.
    pub fn depth_ms(&self, depth: usize) -> u32 {
        (depth / self.per_ms) as u32
    }

    /// Smoothed ring depth in ms — what drift correction actually reacts to, and the honest
    /// number to publish as "audio buffer" (the instantaneous depth swings by a whole quantum).
    pub fn avg_depth_ms(&self) -> u32 {
        (self.depth_avg.max(0.0) as usize / self.per_ms) as u32
    }

    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// The effective target for a device asking for `want` samples per callback. A ring can never
    /// sustain a target below one device quantum, so a large-buffer device (a 20 ms PipeWire graph
    /// quantum, a legacy AAudio path) lifts it to `want` plus one protocol frame rather than
    /// oscillating prime → dropout → re-prime forever.
    fn effective_target(&self, want: usize) -> usize {
        let floor = self.target.max(want + FRAME_MS as usize * self.per_ms);
        match self.sync_target {
            // Continuity outranks sync — see `set_sync_target`. The loop may pull the ring
            // shallower to catch the picture up, or push it deeper when audio runs early, but
            // never below what underrun pressure has proven this link needs, and never past the
            // hard cap that bounds added latency.
            //
            // The ceiling is raised to the floor rather than passed to `clamp` as-is: a device
            // whose callback quantum alone exceeds the preset's `hard_cap_ms` makes `floor > cap`,
            // and `Ord::clamp` PANICS when min > max. That would be a panic in a realtime audio
            // callback on exactly the awkward hardware this code exists to survive — and the same
            // reasoning `step` already applies when it computes its own cap with `.max(target +
            // want)`.
            Some(s) => {
                let cap = (self.tuning.hard_cap_ms as usize * self.per_ms).max(floor);
                s.clamp(floor, cap)
            }
            None => floor,
        }
    }

    /// Decide this callback: what to trim, and whether to play. Call BEFORE reading, with the
    /// ring's current `depth` and the device's `want`, both in interleaved samples.
    pub fn step(&mut self, depth: usize, want: usize) -> JitterStep {
        self.last_want = want;
        let target = self.effective_target(want);

        // Track depth with a callback-rate-independent EWMA: weighting by `want` keeps the time
        // constant at EWMA_TAU_MS whether the device pulls 5 ms or 20 ms at a time.
        let alpha = (want as f32 / (EWMA_TAU_MS as usize * self.per_ms) as f32).clamp(0.0, 1.0);
        self.depth_avg += (depth as f32 - self.depth_avg) * alpha;

        // The hard cap must always leave room to serve this callback, or a large-quantum device
        // would trim itself into a permanent underrun.
        let cap = (target + self.tuning.headroom_ms as usize * self.per_ms)
            .min(self.tuning.hard_cap_ms as usize * self.per_ms)
            .max(target + want);

        let mut out = JitterStep::default();
        if depth > cap {
            // Blew the ceiling: a burst arrived, or we were wedged. Already a discontinuity —
            // discard hard, and reset the drift timer so the trim isn't double-counted as drift.
            out.drop_front = depth - cap;
            self.over_run = 0;
        } else if self.depth_avg
            > (target + self.tuning.shed_excess_ms() as usize * self.per_ms) as f32
        {
            self.over_run += want;
            if self.over_run >= SHED_SUSTAIN_MS as usize * self.per_ms {
                out.drop_front = (FRAME_MS as usize * self.per_ms).min(depth);
                out.crossfade = (SHED_CROSSFADE_MS as usize * self.per_ms)
                    .min(depth.saturating_sub(out.drop_front));
                self.over_run = 0;
            }
        } else {
            self.over_run = 0;
        }
        // Whatever we shed is no longer buffered — reflect it immediately so the next callbacks
        // don't re-fire on a stale average.
        self.depth_avg = (self.depth_avg - out.drop_front as f32).max(0.0);

        if !self.primed && depth.saturating_sub(out.drop_front) >= target {
            self.primed = true;
            self.empties = 0;
        }
        out.silence = !self.primed;
        out
    }

    /// Report the outcome of the read `step` authorised. `ran_short` = the ring could not fill the
    /// callback (a genuine underrun), which drives both the de-prime hysteresis and the adaptive
    /// target floor.
    ///
    /// A callback that `step` told to emit silence is NOT an underrun — the ring is deliberately
    /// re-priming — so calls made while un-primed are ignored and callers need not special-case it.
    pub fn note_read(&mut self, ran_short: bool) {
        if !self.primed {
            return;
        }
        let want = self.last_want.max(1);
        self.window_run += want;
        if self.window_run >= GROW_WINDOW_MS as usize * self.per_ms {
            self.window_run = 0;
            self.underruns = 0;
        }
        if ran_short {
            self.quiet_run = 0;
            self.empties += 1;
            if self.empties >= self.tuning.deprime_after {
                self.primed = false;
                self.empties = 0;
            }
            self.underruns += 1;
            if self.underruns >= GROW_UNDERRUNS {
                // This device genuinely needs more slack than the base target. Grow ONCE per
                // window, capped — the alternative (every device pre-paying the worst device's
                // depth) is what the fixed 40 ms Android floor was.
                self.underruns = 0;
                self.window_run = 0;
                let grown = self.target + GROW_STEP_MS as usize * self.per_ms;
                self.target = grown.min(self.tuning.max_target_ms as usize * self.per_ms);
            }
        } else {
            self.empties = 0;
            self.quiet_run += want;
            // A grown target normally relaxes only after a long quiet spell, because without other
            // evidence the only thing that can justify giving up hard-won slack is time. When the
            // sync loop is asking to run shallower it IS that evidence — a measurement saying the
            // extra depth is costing alignment right now — so test a smaller target sooner. Wrong
            // guesses are cheap and self-correcting: one underrun and the growth path takes it
            // straight back. Without this a ring that ratcheted to the ceiling during a transient
            // would hold the audio a ceiling's worth late for minutes after the cause had gone.
            let quiet_needed = if self.sync_wants_less() {
                SHRINK_QUIET_SYNC_MS
            } else {
                SHRINK_QUIET_MS
            };
            if self.quiet_run >= quiet_needed as usize * self.per_ms {
                // Long quiet spell: give a grown target one step back, so a single bad minute
                // doesn't cost latency for the rest of the session.
                self.quiet_run = 0;
                let base = self.tuning.base_target_ms as usize * self.per_ms;
                self.target = self
                    .target
                    .saturating_sub(GROW_STEP_MS as usize * self.per_ms)
                    .max(base);
            }
        }
    }
}

/// Sample rate of every audio plane in the protocol.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// Discard `drop` interleaved samples from the front of `ring`, linearly crossfading the seam over
/// `fade` samples so a drift correction is inaudible rather than a click.
///
/// The dropped region's tail fades out while the surviving head fades in, so the waveform is
/// continuous across the splice. `fade == 0` discards hard (what a hard-cap trim wants — that
/// backlog is already a discontinuity). Shared by the three `VecDeque<f32>` rings; the Apple ring
/// is index-based and mirrors this in Swift.
pub fn crossfade_drop(ring: &mut std::collections::VecDeque<f32>, drop: usize, fade: usize) {
    if drop == 0 || ring.len() < drop {
        return;
    }
    let fade = fade.min(drop).min(ring.len() - drop);
    if fade == 0 {
        ring.drain(..drop);
        return;
    }
    // The last `fade` samples of what we are about to discard are the fade-OUT source; they blend
    // into the first `fade` samples of what survives.
    let mut faded = Vec::with_capacity(fade);
    for i in 0..fade {
        let old = ring[drop - fade + i];
        let new = ring[drop + i];
        let t = (i + 1) as f32 / (fade + 1) as f32;
        faded.push(old * (1.0 - t) + new * t);
    }
    ring.drain(..drop);
    for (i, v) in faded.into_iter().enumerate() {
        ring[i] = v;
    }
}

// ---- per-platform channel-layout helpers (pure data; no platform deps) --------------------

/// Windows `WAVEFORMATEXTENSIBLE.dwChannelMask` for the wire layout.
///
/// NB 7.1 == `0x63F` (FL FR FC LFE **BL BR SL SR**), NOT `0xFF` — `0xFF` selects the
/// front-of-center pair FLC/FRC, the wrong speakers. WASAPI delivers channels in ascending
/// mask-bit order, which equals the wire order, so the decoded PCM needs no permutation.
pub const fn wasapi_channel_mask(channels: u8) -> u32 {
    const FL: u32 = 0x1;
    const FR: u32 = 0x2;
    const FC: u32 = 0x4;
    const LFE: u32 = 0x8;
    const BL: u32 = 0x10; // back left  (wire RL)
    const BR: u32 = 0x20; // back right (wire RR)
    const SL: u32 = 0x200; // side left
    const SR: u32 = 0x400; // side right
    match channels {
        6 => FL | FR | FC | LFE | BL | BR,           // 0x3F
        8 => FL | FR | FC | LFE | BL | BR | SL | SR, // 0x63F
        _ => FL | FR,                                // 0x3 (stereo)
    }
}

/// PipeWire / SPA `enum spa_audio_channel` positions in wire order — identical to the host
/// capture side (`punktfunk-host` `audio::linux::spa_positions`): FL=3 FR=4 FC=5 LFE=6 SL=7
/// SR=8 RL=12 RR=13. Identity routing: the client sets these on its playback node so PipeWire
/// maps each wire slot to the matching speaker (and downmixes when the sink has fewer).
pub fn spa_positions(channels: u8) -> &'static [u32] {
    const STEREO: [u32; 2] = [3, 4]; // FL FR
    const C51: [u32; 6] = [3, 4, 5, 6, 12, 13]; // FL FR FC LFE RL RR
    const C71: [u32; 8] = [3, 4, 5, 6, 12, 13, 7, 8]; // FL FR FC LFE RL RR SL SR
    match channels {
        6 => &C51,
        8 => &C71,
        _ => &STEREO,
    }
}

/// The lock-free hand-off between the thread that knows the TIMESTAMPS (the decode/pull thread,
/// which sees each packet's `pts_ns`) and the one that knows the RING (the realtime audio
/// callback, which owns the depth and the [`JitterPolicy`]). Neither can do the job alone and the
/// callback must not block, so they trade two words.
///
/// `usize::MAX` encodes "no target" rather than `0`, because `0` is a perfectly ordinary depth to
/// ask for and conflating the two would silently mean "run the ring dry".
#[derive(Debug)]
pub struct AudioSyncCell {
    depth: std::sync::atomic::AtomicUsize,
    target: std::sync::atomic::AtomicUsize,
}

impl Default for AudioSyncCell {
    fn default() -> Self {
        AudioSyncCell {
            depth: std::sync::atomic::AtomicUsize::new(0),
            target: std::sync::atomic::AtomicUsize::new(usize::MAX),
        }
    }
}

impl AudioSyncCell {
    /// Callback side: publish the ring's current depth in interleaved samples.
    pub fn publish_depth(&self, depth: usize) {
        self.depth
            .store(depth, std::sync::atomic::Ordering::Relaxed);
    }

    /// Decode side: the ring depth as last seen by the audio callback.
    pub fn depth(&self) -> usize {
        self.depth.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Decode side: ask the ring to aim for this depth (`None` = run unsynchronised).
    pub fn set_target(&self, target: Option<usize>) {
        self.target.store(
            target.unwrap_or(usize::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Callback side: the depth the sync loop wants, if any.
    pub fn target(&self) -> Option<usize> {
        match self.target.load(std::sync::atomic::Ordering::Relaxed) {
            usize::MAX => None,
            t => Some(t),
        }
    }
}

/// Smoothing time constant for the measured A/V offset, in ms of consumed audio. Long enough that
/// network jitter and a single late datagram do not move it; short enough to track real drift.
const AV_EWMA_TAU_MS: u32 = 2_000;
/// Offsets inside this band are left alone. Correcting a few ms costs a (crossfaded, but real)
/// discontinuity and buys nothing a listener can perceive — detectability for A/V misalignment sits
/// an order of magnitude above it. The deadband is what keeps the loop from hunting forever around
/// zero, which would be audible in a way the misalignment it was chasing was not.
const AV_DEADBAND_MS: u32 = 10;
/// Observations folded before the first correction is offered. The offset is derived from a clock
/// skew estimate and a video figure that both need a moment to settle after connect; acting on the
/// first sample would chase the handshake, not the stream.
const AV_MIN_OBSERVATIONS: u32 = 100;
/// An offset larger than this is not believed. A wall-clock step, a paused host, or a stale video
/// figure can all produce an enormous apparent misalignment, and steering the ring by it would
/// empty or overfill it outright. Beyond this the loop reports and waits rather than acting.
const AV_SANE_LIMIT_MS: u32 = 1_000;

/// The A/V synchronisation controller: turns "when will this audio actually play" and "when did the
/// picture it belongs with reach the glass" into a ring depth the [`JitterPolicy`] should aim for.
///
/// **The defect it exists to fix.** The host stamps `pts_ns` on every audio datagram and the client
/// decoded it into `AudioPacket` — and then never read it. Video's `pts_ns`, by contrast, is used
/// end to end (the presenter computes a true glass-to-glass `displayed + clock_offset − pts`). So
/// audio free-ran at whatever depth its jitter ring happened to settle at, video was presented on a
/// wholly independent path, and nothing ever compared them: the A/V offset was an accident of
/// buffer depths. It moved whenever the ring ratcheted under underrun pressure, and — the way this
/// surfaced in the field — it got WORSE every time video got faster, because a quicker decoder
/// lowers the video leg while leaving the audio leg exactly where it was.
///
/// **Video is the master.** In a game streamer the video leg is the input-feel budget and must
/// never be inflated to satisfy the audio clock; audio tolerates small, crossfaded, rate-limited
/// corrections that are inaudible, and [`crossfade_drop`] already applies them. So audio moves.
///
/// **Continuity outranks sync.** This type only ever proposes a depth. [`JitterPolicy`] clamps the
/// proposal to its own underrun-driven floor, so a link whose jitter genuinely needs more buffer
/// than the picture is away keeps its buffer and the residual is reported instead of being taken
/// out of the listener's stream. See [`JitterPolicy::set_sync_target`].
#[derive(Clone, Debug)]
pub struct AvSync {
    /// Interleaved samples per millisecond at the negotiated layout (48 × channels).
    per_ms: usize,
    /// EWMA of the measured offset in ns. Positive = audio is scheduled to play LATE relative to
    /// the picture it belongs with.
    offset_avg_ns: f32,
    observations: u32,
    /// Set once an observation lands outside [`AV_SANE_LIMIT_MS`], for reporting.
    implausible: bool,
}

/// One measurement handed to [`AvSync::observe`]. Every field is in the units its source already
/// produces, so no caller has to do clock arithmetic to use it correctly.
#[derive(Clone, Copy, Debug)]
pub struct AvSyncObservation {
    /// The host capture timestamp carried by the audio frame being queued (host clock).
    pub pts_ns: u64,
    /// Local wall-clock now, same basis the client's video latency math uses (CLOCK_REALTIME).
    pub now_local_ns: i128,
    /// Host clock minus client clock, from the skew handshake (`clock_offset_now_ns`).
    pub clock_offset_ns: i64,
    /// How much audio is already queued AHEAD of this frame, in interleaved samples — everything
    /// that must play before it does.
    pub buffered_ahead: usize,
    /// The video plane's current end-to-end figure in ns: `displayed + clock_offset − pts`, as the
    /// presenter already computes it. `None` while no frame has been presented yet.
    pub video_e2e_ns: Option<u64>,
}

impl AvSync {
    /// `channels` is the negotiated interleaved channel count (2/6/8).
    pub fn new(channels: u8) -> AvSync {
        AvSync {
            per_ms: (SAMPLE_RATE_HZ / 1000) as usize * channels.max(1) as usize,
            offset_avg_ns: 0.0,
            observations: 0,
            implausible: false,
        }
    }

    /// Fold one measurement. Returns the smoothed offset in ns once there is enough evidence to
    /// believe it (positive = audio late), or `None` while still settling.
    ///
    /// Rejecting the implausible rather than clamping it is deliberate: a wall-clock step or a
    /// stale video figure produces a huge apparent offset, and a clamped-but-wrong value would be
    /// acted on as though it were a small real one.
    pub fn observe(&mut self, o: AvSyncObservation) -> Option<i64> {
        // No frame on the glass yet ⇒ no reference to align against, so nothing to say.
        let video_e2e_ns = o.video_e2e_ns?;
        // When this frame's samples will actually reach the speaker, expressed in the host's
        // capture clock — the same clock, and the same shape, as the video figure it is compared
        // against.
        let buffered_ns = (o.buffered_ahead / self.per_ms.max(1)) as i128 * 1_000_000;
        let play_at_host = o.now_local_ns + buffered_ns + o.clock_offset_ns as i128;
        let audio_e2e_ns = play_at_host - o.pts_ns as i128;
        let offset_ns = audio_e2e_ns - video_e2e_ns as i128;

        if offset_ns.unsigned_abs() > (AV_SANE_LIMIT_MS as u128) * 1_000_000 {
            self.implausible = true;
            return None;
        }
        self.implausible = false;

        // Weight by one protocol frame so the time constant means the same thing regardless of how
        // often the caller observes.
        let alpha = (FRAME_MS as f32 / AV_EWMA_TAU_MS as f32).clamp(0.0, 1.0);
        if self.observations == 0 {
            self.offset_avg_ns = offset_ns as f32;
        } else {
            self.offset_avg_ns += (offset_ns as f32 - self.offset_avg_ns) * alpha;
        }
        self.observations = self.observations.saturating_add(1);
        self.settled().then_some(self.offset_avg_ns as i64)
    }

    /// Enough evidence folded to act on.
    pub fn settled(&self) -> bool {
        self.observations >= AV_MIN_OBSERVATIONS
    }

    /// The smoothed offset in ms (positive = audio late), for the HUD. Reported as soon as it is
    /// measured, including while still settling — a number the operator can watch converge is more
    /// useful than a blank that hides whether the loop is working at all.
    pub fn offset_ms(&self) -> i32 {
        (self.offset_avg_ns / 1_000_000.0) as i32
    }

    /// The last observation was outside the believable range and was discarded.
    pub fn implausible(&self) -> bool {
        self.implausible
    }

    /// The ring depth that would place audio with the picture, given where the ring is now.
    /// `None` while unsettled or inside the deadband — the caller then leaves the policy alone.
    ///
    /// Audio late (offset > 0) means there is too much queued: aim shallower. Audio early means
    /// aim deeper.
    pub fn desired_depth(&self, current_depth: usize) -> Option<usize> {
        if !self.settled() {
            return None;
        }
        let offset_ms = self.offset_avg_ns / 1_000_000.0;
        if offset_ms.abs() < AV_DEADBAND_MS as f32 {
            return None;
        }
        let delta = (offset_ms * self.per_ms as f32) as i64;
        Some((current_depth as i64 - delta).max(0) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_table_is_consistent() {
        for l in [
            &LAYOUT_STEREO,
            &LAYOUT_51,
            &LAYOUT_51_HQ,
            &LAYOUT_71,
            &LAYOUT_71_HQ,
        ] {
            // Mapping is identity and exactly `channels` entries long.
            assert_eq!(l.mapping.len(), l.channels as usize);
            for (i, &m) in l.mapping.iter().enumerate() {
                assert_eq!(m as usize, i, "mapping must be identity for {l:?}");
            }
            // libopus invariant: total channels == coupled*2 + (streams - coupled).
            assert_eq!(
                l.coupled * 2 + (l.streams - l.coupled),
                l.channels,
                "stream/coupled accounting for {l:?}"
            );
            assert!(l.coupled <= l.streams);
            assert!(l.bitrate > 0);
        }
    }

    #[test]
    fn layout_for_picks_expected() {
        assert_eq!(layout_for(2, false), &LAYOUT_STEREO);
        assert_eq!(layout_for(6, false), &LAYOUT_51);
        assert_eq!(layout_for(6, true), &LAYOUT_51_HQ);
        assert_eq!(layout_for(8, false), &LAYOUT_71);
        assert_eq!(layout_for(8, true), &LAYOUT_71_HQ);
        // Unknown / 0 → stereo.
        assert_eq!(layout_for(0, false), &LAYOUT_STEREO);
        assert_eq!(layout_for(3, false), &LAYOUT_STEREO);
        assert_eq!(layout_for(7, true), &LAYOUT_STEREO);
    }

    #[test]
    fn normalize_clamps_to_negotiable() {
        assert_eq!(normalize_channels(2), 2);
        assert_eq!(normalize_channels(6), 6);
        assert_eq!(normalize_channels(8), 8);
        for bad in [0u8, 1, 3, 4, 5, 7, 9, 255] {
            assert_eq!(normalize_channels(bad), 2, "{bad} must clamp to stereo");
        }
    }

    #[test]
    fn gap_tracker_counts_only_forward_gaps() {
        let mut t = AudioGapTracker::new();
        assert_eq!(t.missing_before(100), 0, "first packet");
        assert_eq!(t.missing_before(101), 0, "in order");
        assert_eq!(t.missing_before(104), 2, "102+103 lost");
        assert_eq!(t.missing_before(104), 0, "duplicate");
        assert_eq!(t.missing_before(103), 0, "late reorder conceals nothing");
        assert_eq!(t.missing_before(105), 0, "reorder didn't move the anchor");
        // A huge gap is capped; the stream continues from the new anchor.
        assert_eq!(t.missing_before(105 + 1000), MAX_CONCEAL_PACKETS);
        assert_eq!(t.missing_before(105 + 1001), 0);
    }

    #[test]
    fn gap_tracker_survives_seq_wraparound() {
        let mut t = AudioGapTracker::new();
        assert_eq!(t.missing_before(u32::MAX - 1), 0);
        assert_eq!(t.missing_before(u32::MAX), 0, "in order at the edge");
        assert_eq!(t.missing_before(1), 1, "seq 0 lost across the wrap");
        assert_eq!(t.missing_before(0), 0, "pre-wrap reorder, not a 2^31 gap");
    }

    // ---- redundant-plane recovery ---------------------------------------------------------

    #[test]
    fn red_recovery_rebuilds_exactly_the_single_missing_frame() {
        let mut r = AudioRedRecovery::new();
        // First packet: nothing is known to be missing, so nothing is prepended.
        assert!(!r.recover_before(10, true));
        // In order.
        assert!(!r.recover_before(11, true));
        // 12 lost: 13 carries it.
        assert!(r.recover_before(13, true));
        // Back in order from the new anchor.
        assert!(!r.recover_before(14, true));
    }

    #[test]
    fn red_recovery_is_conservative() {
        let mut r = AudioRedRecovery::new();
        r.recover_before(10, true);
        // A datagram with no redundant copy recovers nothing, however big the gap.
        assert!(!r.recover_before(20, false));
        // Duplicates and reorders recover nothing, and must not move the anchor backwards.
        let mut r = AudioRedRecovery::new();
        r.recover_before(10, true);
        r.recover_before(11, true);
        assert!(!r.recover_before(11, true), "duplicate");
        assert!(!r.recover_before(9, true), "late reorder");
        assert!(
            !r.recover_before(12, true),
            "the reorder must not have moved the anchor"
        );
    }

    /// A longer burst still recovers its last frame — the gap the client has to conceal gets one
    /// frame shorter, which is strictly better than concealing all of it.
    #[test]
    fn red_recovery_shortens_a_longer_burst() {
        let mut r = AudioRedRecovery::new();
        r.recover_before(100, true);
        assert!(
            r.recover_before(105, true),
            "104 is recoverable even though 101-103 are not"
        );
    }

    #[test]
    fn red_recovery_survives_seq_wraparound() {
        let mut r = AudioRedRecovery::new();
        assert!(!r.recover_before(u32::MAX - 1, true));
        assert!(
            !r.recover_before(u32::MAX, true),
            "in order across the edge"
        );
        assert!(r.recover_before(1, true), "seq 0 lost across the wrap");
        assert!(!r.recover_before(2, true));
    }

    /// The two halves must agree: whatever `AudioRedRecovery` rebuilds, `AudioGapTracker` must
    /// then see as no gap at all — that is the whole point of doing recovery on the demux side.
    #[test]
    fn recovery_and_the_gap_tracker_agree() {
        let mut rec = AudioRedRecovery::new();
        let mut gaps = AudioGapTracker::new();
        let mut concealed = 0;
        // Deliver 0..20 with 7 and 13 lost; each survivor carries its predecessor.
        let mut emitted: Vec<u32> = Vec::new();
        for seq in (0..20u32).filter(|s| *s != 7 && *s != 13) {
            if rec.recover_before(seq, true) {
                emitted.push(seq - 1);
            }
            emitted.push(seq);
        }
        for seq in &emitted {
            concealed += gaps.missing_before(*seq);
        }
        assert_eq!(
            concealed, 0,
            "recovered stream must need no concealment: {emitted:?}"
        );
        assert_eq!(emitted.len(), 20, "every frame accounted for");
        assert!(
            emitted.windows(2).all(|w| w[1] == w[0] + 1),
            "and in order: {emitted:?}"
        );
    }

    // ---- bitrate tiers -------------------------------------------------------------------

    /// `Standard` must reproduce the historical table EXACTLY — that is what makes the tier
    /// machinery provably non-regressive against every pre-tier build.
    #[test]
    fn standard_tier_is_the_legacy_table() {
        for l in [
            &LAYOUT_STEREO,
            &LAYOUT_51,
            &LAYOUT_51_HQ,
            &LAYOUT_71,
            &LAYOUT_71_HQ,
        ] {
            assert_eq!(l.bitrate_for(AudioTier::Standard), l.bitrate, "{l:?}");
        }
    }

    #[test]
    fn tiers_are_monotonic_and_hq_layouts_are_invariant() {
        for l in [&LAYOUT_STEREO, &LAYOUT_51, &LAYOUT_71] {
            let (lo, std, hi) = (
                l.bitrate_for(AudioTier::Low),
                l.bitrate_for(AudioTier::Standard),
                l.bitrate_for(AudioTier::High),
            );
            assert!(lo < std && std < hi, "{l:?}: {lo} < {std} < {hi}");
        }
        // The uncoupled HQ layouts are already past transparency — no tier may move them.
        for l in [&LAYOUT_51_HQ, &LAYOUT_71_HQ] {
            for t in [AudioTier::Low, AudioTier::Standard, AudioTier::High] {
                assert_eq!(l.bitrate_for(t), l.bitrate, "{l:?} at {t:?}");
            }
        }
    }

    #[test]
    fn tier_default_is_high_and_parses() {
        assert_eq!(AudioTier::default(), AudioTier::High);
        for t in [AudioTier::Low, AudioTier::Standard, AudioTier::High] {
            assert_eq!(AudioTier::parse(t.as_str()), Some(t));
        }
        assert_eq!(AudioTier::parse("  HIGH "), Some(AudioTier::High));
        assert_eq!(AudioTier::parse("normal"), Some(AudioTier::Standard));
        // Unknown spellings must be rejected, not silently downgraded.
        assert_eq!(AudioTier::parse("transparent"), None);
        assert_eq!(AudioTier::parse(""), None);
    }

    // ---- the audio bandwidth budget --------------------------------------------------------

    /// THE regression this guards: `High` (256 kbps stereo) and the redundant plane (x2) were
    /// each justified as "~1 % of the video budget" and nobody added them together. 512 kbps is
    /// ~10 % of a 5 Mbps session — and audio is outside the ABR loop, so ABR cannot reclaim it.
    #[test]
    fn budget_steps_down_as_the_link_narrows() {
        let plan = |kbps| plan_audio_budget(kbps, 2, AudioTier::High, true);
        // Roomy link: everything on.
        let b = plan(20_000);
        assert_eq!((b.tier, b.redundancy), (AudioTier::High, true));
        assert_eq!(b.kbps, 512);
        // Halve it and redundancy is the first thing to go — quality is what the field report
        // was about, and redundancy only pays under loss.
        assert_eq!(plan(10_000).tier, AudioTier::High);
        assert!(!plan(10_000).redundancy);
        // Tighter still: down to Standard.
        assert_eq!(plan(5_000).tier, AudioTier::Standard);
        assert!(!plan(5_000).redundancy);
        // A genuinely narrow link lands on Low, and never below it.
        assert_eq!(plan(1_000).tier, AudioTier::Low);
        assert_eq!(plan(1).tier, AudioTier::Low);
        assert_eq!(
            plan(0).kbps,
            96,
            "audio must survive an absurd video bitrate"
        );
    }

    /// The budget must never spend more than its share, at any bitrate or channel count.
    #[test]
    fn budget_never_exceeds_its_share() {
        for kbps in [0u32, 500, 1_000, 2_000, 5_000, 10_000, 20_000, 100_000] {
            for ch in [2u8, 6, 8] {
                let b = plan_audio_budget(kbps, ch, AudioTier::High, true);
                let allowed =
                    (kbps.saturating_mul(AUDIO_BUDGET_PCT) / 100).max(AUDIO_BUDGET_FLOOR_KBPS);
                let floor = plan_audio_budget(0, ch, AudioTier::Low, false).kbps;
                assert!(
                    b.kbps <= allowed || b.kbps == floor,
                    "{ch}ch at {kbps} kbps: spent {} of {allowed}",
                    b.kbps
                );
            }
        }
    }

    /// Surround costs more per tier, so the same link must step it down sooner than stereo —
    /// the budget is about total wire cost, not about the tier name.
    #[test]
    fn budget_accounts_for_the_channel_count() {
        let stereo = plan_audio_budget(10_000, 2, AudioTier::High, true);
        let surround = plan_audio_budget(10_000, 8, AudioTier::High, true);
        assert_eq!(stereo.tier, AudioTier::High);
        assert!(surround.kbps <= stereo.kbps.max(surround.kbps), "sanity");
        // 7.1 at High is 768 kbps — far past a 500 kbps allowance, so it must have stepped down.
        assert!(
            surround.kbps < 768,
            "7.1 High must not fit a 10 Mbps budget"
        );
    }

    /// The budget may LOWER what was asked for, never raise it: an operator who set `low` gets
    /// `low` on a 100 Mbps link, and a client that never asked for redundancy never gets it.
    #[test]
    fn budget_respects_the_request() {
        let b = plan_audio_budget(100_000, 2, AudioTier::Low, true);
        assert_eq!(b.tier, AudioTier::Low);
        let b = plan_audio_budget(100_000, 2, AudioTier::Standard, true);
        assert_eq!(b.tier, AudioTier::Standard);
        assert!(b.redundancy, "Standard + redundancy fits a huge link");
        let b = plan_audio_budget(100_000, 2, AudioTier::High, false);
        assert_eq!(b.tier, AudioTier::High);
        assert!(
            !b.redundancy,
            "a client that did not ask must never be sent 0xD2"
        );
    }

    // ---- the de-jitter policy ------------------------------------------------------------

    /// Interleaved samples per ms at `channels`.
    fn per_ms(channels: u8) -> usize {
        (SAMPLE_RATE_HZ / 1000) as usize * channels as usize
    }

    /// One simulated run's outcome.
    #[derive(Debug, Default)]
    struct Sim {
        final_ms: u32,
        peak_ms: u32,
        /// Smooth drift corrections (crossfaded, one frame each) — the good kind.
        soft_sheds: u32,
        /// Hard-cap trims — the backstop. Any of these in a plain-drift run means the smooth
        /// correction is not doing its job.
        hard_trims: u32,
        underruns: u32,
    }

    /// Drive a policy through `ms` of simulated audio at a `quantum_ms` device, where the producer
    /// delivers `drift_ppm` more (or less) than the consumer takes — i.e. host-vs-client clock skew.
    fn simulate(
        tuning: JitterTuning,
        channels: u8,
        ms: u32,
        quantum_ms: u32,
        drift_ppm: i64,
        start_ms: u32,
    ) -> Sim {
        let pm = per_ms(channels);
        let want = quantum_ms as usize * pm;
        let mut p = JitterPolicy::new(tuning, channels);
        let mut depth = start_ms as usize * pm;
        let mut out = Sim::default();
        // Fractional producer accumulator, so a sub-sample-per-callback drift still accumulates.
        let mut carry: i64 = 0;
        for _ in 0..(ms / quantum_ms) {
            // Producer: one quantum of audio plus the drift.
            carry += want as i64 * drift_ppm;
            let extra = carry / 1_000_000;
            carry -= extra * 1_000_000;
            depth = (depth as i64 + want as i64 + extra).max(0) as usize;

            let s = p.step(depth, want);
            if s.drop_front > 0 {
                if s.crossfade > 0 {
                    out.soft_sheds += 1;
                } else {
                    out.hard_trims += 1;
                }
                depth -= s.drop_front.min(depth);
            }
            if s.silence {
                p.note_read(false);
                continue;
            }
            let short = depth < want;
            depth -= want.min(depth);
            if short {
                out.underruns += 1;
            }
            p.note_read(short);
            out.peak_ms = out.peak_ms.max((depth / pm) as u32);
        }
        out.final_ms = (depth / pm) as u32;
        out
    }

    /// The invariant that makes drift correction real rather than decorative: on every preset the
    /// smooth shed point must sit strictly BELOW the hard trim point. Invert it — by tuning
    /// `headroom_ms` down — and the ring is trimmed back before the depth average can ever reach
    /// the shed threshold, so the smooth path becomes dead code and every correction is the
    /// audible drop it was meant to replace. (That inversion was present in the first draft of
    /// this module and only surfaced because `a_transient_burst_does_not_shed` failed.)
    #[test]
    fn every_preset_sheds_before_it_trims() {
        for (name, t) in [
            ("PIPEWIRE", JitterTuning::PIPEWIRE),
            ("WASAPI", JitterTuning::WASAPI),
            ("COREAUDIO", JitterTuning::COREAUDIO),
            ("AAUDIO", JitterTuning::AAUDIO),
        ] {
            assert!(
                t.shed_excess_ms() < t.headroom_ms,
                "{name}: sheds at +{} ms but trims at +{} ms — drift correction can never fire",
                t.shed_excess_ms(),
                t.headroom_ms
            );
            assert!(
                t.base_target_ms + t.headroom_ms <= t.hard_cap_ms,
                "{name}: the headroom band is cut short by the hard cap"
            );
            assert!(t.max_target_ms >= t.base_target_ms, "{name}");
            assert!(t.deprime_after >= 2, "{name}: needs real hysteresis");
        }
    }

    /// THE headline behaviour, and the defect this policy exists for: with the host clock running
    /// fast, the old rings grew to their ceiling and stayed pinned there for the rest of the
    /// session. Drift correction must hold the depth near target — and must do it with the SMOOTH
    /// crossfaded shed, never by letting the hard cap chop the backlog.
    #[test]
    fn drift_does_not_ratchet_latency_to_the_ceiling() {
        // +200 ppm is a deliberately harsh skew (real DAC pairs are tens of ppm); 5 minutes.
        let s = simulate(JitterTuning::AAUDIO, 2, 300_000, 5, 200, 25);
        assert!(
            s.soft_sheds > 0,
            "drift must be shed, not accumulated: {s:?}"
        );
        assert_eq!(
            s.hard_trims, 0,
            "plain drift must never reach the hard cap: {s:?}"
        );
        assert_eq!(
            s.underruns, 0,
            "shedding must never cause an underrun: {s:?}"
        );
        // The old Android ring pinned at its 120 ms hard cap. Ours must stay inside the band.
        let ceiling = JitterTuning::AAUDIO.base_target_ms + JitterTuning::AAUDIO.headroom_ms;
        assert!(
            s.peak_ms <= ceiling,
            "peaked at {} ms (band ends at {ceiling}) — that is the ratchet, not a correction",
            s.peak_ms
        );
    }

    /// Same skew, every preset: none of them may ratchet.
    #[test]
    fn no_preset_ratchets_under_drift() {
        for (name, t) in [
            ("PIPEWIRE", JitterTuning::PIPEWIRE),
            ("WASAPI", JitterTuning::WASAPI),
            ("COREAUDIO", JitterTuning::COREAUDIO),
            ("AAUDIO", JitterTuning::AAUDIO),
        ] {
            let s = simulate(t, 2, 300_000, 5, 200, t.base_target_ms);
            assert!(s.soft_sheds > 0, "{name}: {s:?}");
            assert!(
                s.peak_ms <= t.base_target_ms + t.headroom_ms,
                "{name} peaked at {} ms: {s:?}",
                s.peak_ms
            );
        }
    }

    /// The mirror case: a host clock running SLOW must not be "corrected" into permanent
    /// underruns. The adaptive floor may grow the target, but nothing may be shed.
    #[test]
    fn negative_drift_grows_the_target_instead_of_stuttering() {
        let s = simulate(JitterTuning::AAUDIO, 2, 120_000, 5, -200, 25);
        assert_eq!(
            s.soft_sheds, 0,
            "nothing to shed when the ring is draining: {s:?}"
        );
        assert_eq!(s.hard_trims, 0, "{s:?}");
    }

    /// A shed must never fire on a transient — a burst that arrives and drains is normal jitter,
    /// and shedding it would cost an audible artefact for nothing. The spike here sits ABOVE the
    /// shed threshold but below the trim point, so only the sustain requirement can reject it.
    #[test]
    fn a_transient_burst_does_not_shed() {
        let t = JitterTuning::AAUDIO;
        let pm = per_ms(2);
        let want = 5 * pm;
        let spike_ms = t.base_target_ms + t.shed_excess_ms() + FRAME_MS; // inside the band
        assert!(
            spike_ms < t.base_target_ms + t.headroom_ms,
            "test spike must not hit the trim"
        );
        let mut p = JitterPolicy::new(t, 2);
        let mut sheds = 0;
        // 300 ms spiked out of every 1 s, for 20 s.
        for round in 0..20 {
            for i in 0..200 {
                let depth = if round > 0 && i < 60 {
                    spike_ms
                } else {
                    t.base_target_ms
                } as usize;
                let s = p.step(depth * pm, want);
                if s.drop_front > 0 {
                    sheds += 1;
                }
                p.note_read(false);
            }
        }
        assert_eq!(
            sheds, 0,
            "a repeated short burst must not trigger drift correction"
        );
    }

    /// The hard cap is the only absolute latency guarantee — it trims immediately, without
    /// waiting for the drift timer.
    #[test]
    fn hard_cap_trims_at_once() {
        let pm = per_ms(2);
        let mut p = JitterPolicy::new(JitterTuning::AAUDIO, 2);
        let s = p.step(500 * pm, 5 * pm);
        assert!(
            s.drop_front > 0,
            "a 500 ms backlog must be trimmed on the spot"
        );
        assert_eq!(s.crossfade, 0, "a blown cap is already a discontinuity");
        let left = 500 * pm - s.drop_front;
        assert!(
            left <= JitterTuning::AAUDIO.hard_cap_ms as usize * pm,
            "trim must land at or under the hard cap"
        );
    }

    /// One transient drain must not manufacture a fresh target's worth of silence — the bug
    /// Android fixed and Linux/Windows still carried.
    #[test]
    fn deprime_requires_hysteresis() {
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
        // An EMPTY ring must emit silence and stay un-primed, however many callbacks it sees.
        for _ in 0..10 {
            assert!(p.step(0, want).silence, "an empty ring cannot play");
        }
        assert!(!p.is_primed());
        // A ring already holding well over target primes on the first callback that sees it.
        assert!(
            !p.step(50 * pm, want).silence,
            "a ring holding well over target must start immediately"
        );
        assert!(p.is_primed());
        p.note_read(true); // one short read
        assert!(p.is_primed(), "a single short read must not de-prime");
        for _ in 1..JitterTuning::PIPEWIRE.deprime_after {
            p.note_read(true);
        }
        assert!(!p.is_primed(), "a sustained drain must re-prime");
    }

    /// A device that pulls a big quantum cannot sustain a target below it: the effective target
    /// must lift, or the ring oscillates prime → dropout → re-prime forever.
    #[test]
    fn target_lifts_above_a_large_device_quantum() {
        let pm = per_ms(2);
        let mut p = JitterPolicy::new(JitterTuning::PIPEWIRE, 2); // base target 15 ms
        let want = 40 * pm; // a 40 ms graph quantum — far above the base target
                            // At exactly the base target the ring must NOT claim to be primed.
        assert!(
            p.step(15 * pm, want).silence,
            "15 ms cannot serve a 40 ms quantum"
        );
        // Once it holds the quantum plus a frame, it may play.
        let s = p.step((40 + FRAME_MS as usize) * pm, want);
        assert!(!s.silence, "quantum + one frame must be enough to start");
    }

    /// Clustered underruns raise the floor (that device needs the slack); a long quiet spell
    /// gives it back, so one bad minute doesn't cost latency for the whole session.
    #[test]
    fn target_grows_on_underruns_and_relaxes_when_quiet() {
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(JitterTuning::AAUDIO, 2);
        let base = p.target_ms();
        assert_eq!(base, JitterTuning::AAUDIO.base_target_ms);
        for _ in 0..40 {
            // Keep it primed and starve it: depth is always enough to prime, never to serve.
            while !p.is_primed() {
                p.step(200 * pm, want);
            }
            p.step(200 * pm, want);
            p.note_read(true);
        }
        let grown = p.target_ms();
        assert!(
            grown > base,
            "clustered underruns must raise the floor ({base} → {grown})"
        );
        assert!(
            grown <= JitterTuning::AAUDIO.max_target_ms,
            "growth must respect max_target_ms"
        );
        // Now a long clean run relaxes it back.
        for _ in 0..(SHRINK_QUIET_MS as usize * 3 / 5) {
            p.step(grown as usize * pm + want, want);
            p.note_read(false);
        }
        assert!(
            p.target_ms() < grown,
            "a quiet spell must give the growth back"
        );
        assert!(p.target_ms() >= base, "…but never below the base target");
    }

    /// The crossfade must leave a continuous waveform: splicing a ramp must not introduce a step
    /// bigger than the ramp's own per-sample slope.
    #[test]
    fn crossfade_drop_splices_without_a_step() {
        use std::collections::VecDeque;
        // A slow ramp: any hard splice shows up as a visible jump.
        let mut ring: VecDeque<f32> = (0..1000).map(|i| i as f32).collect();
        let (drop, fade) = (240, 96);
        crossfade_drop(&mut ring, drop, fade);
        assert_eq!(ring.len(), 1000 - drop);
        // Across the whole faded region the step between neighbours stays bounded — a hard drop
        // would show a `drop`-sized jump at index 0.
        for i in 0..fade {
            let step = (ring[i + 1] - ring[i]).abs();
            assert!(
                step < drop as f32,
                "sample {i}: step {step} looks like a hard splice"
            );
        }
        // Tail is untouched.
        assert_eq!(ring[ring.len() - 1], 999.0);
    }

    #[test]
    fn crossfade_drop_handles_degenerate_inputs() {
        use std::collections::VecDeque;
        let mut ring: VecDeque<f32> = (0..10).map(|i| i as f32).collect();
        crossfade_drop(&mut ring, 0, 4); // nothing to drop
        assert_eq!(ring.len(), 10);
        crossfade_drop(&mut ring, 99, 4); // more than we hold — refuse
        assert_eq!(ring.len(), 10);
        crossfade_drop(&mut ring, 10, 4); // exactly all of it: no room to fade, hard drop
        assert!(ring.is_empty());
    }

    #[test]
    fn wasapi_masks_are_correct() {
        assert_eq!(wasapi_channel_mask(2), 0x3);
        assert_eq!(wasapi_channel_mask(6), 0x3F);
        assert_eq!(wasapi_channel_mask(8), 0x63F); // NOT 0xFF
                                                   // Bit count must equal the channel count.
        assert_eq!(wasapi_channel_mask(2).count_ones(), 2);
        assert_eq!(wasapi_channel_mask(6).count_ones(), 6);
        assert_eq!(wasapi_channel_mask(8).count_ones(), 8);
    }

    #[test]
    fn spa_positions_match_wire_order() {
        assert_eq!(spa_positions(2), &[3, 4]);
        assert_eq!(spa_positions(6), &[3, 4, 5, 6, 12, 13]);
        assert_eq!(spa_positions(8), &[3, 4, 5, 6, 12, 13, 7, 8]);
        assert_eq!(spa_positions(2).len(), 2);
        assert_eq!(spa_positions(6).len(), 6);
        assert_eq!(spa_positions(8).len(), 8);
    }

    /// Real-libopus proof that the shared layout round-trips with channel identity: a tone fed
    /// into wire channel N (host `opus::MSEncoder`) comes back out on channel N (client
    /// `opus::MSDecoder`), for stereo / 5.1 / 7.1. This is the single guarantee the whole
    /// feature rests on — encoder layout == decoder layout == identity mapping — so if a layout
    /// constant is ever wrong, this fails. Gated on `quic` (where `opus` is a dependency).
    #[cfg(feature = "quic")]
    #[test]
    fn multistream_layout_roundtrips_with_channel_identity() {
        const SR: u32 = 48_000;
        const SAMPLES: usize = 240; // 5 ms @ 48 kHz
        for &channels in &[2u8, 6, 8] {
            let l = layout_for(channels, false);
            let ch = l.channels as usize;
            let mut enc = opus::MSEncoder::new(
                SR,
                l.streams,
                l.coupled,
                l.mapping,
                opus::Application::LowDelay,
            )
            .expect("MSEncoder");
            enc.set_bitrate(opus::Bitrate::Bits(l.bitrate)).unwrap();
            enc.set_vbr(false).unwrap();
            let mut dec =
                opus::MSDecoder::new(SR, l.streams, l.coupled, l.mapping).expect("MSDecoder");

            for tone_ch in 0..ch {
                let mut out = vec![0u8; 4000];
                let mut energy = vec![0f64; ch];
                // A few frames to clear the codec startup transient before measuring.
                for f in 0..8 {
                    let mut frame = vec![0f32; SAMPLES * ch];
                    for t in 0..SAMPLES {
                        let phase = (f * SAMPLES + t) as f32 * 440.0 * 2.0 * std::f32::consts::PI
                            / SR as f32;
                        frame[t * ch + tone_ch] = 0.5 * phase.sin();
                    }
                    let n = enc.encode_float(&frame, &mut out).unwrap();
                    let mut decoded = vec![0f32; SAMPLES * ch];
                    let got = dec.decode_float(&out[..n], &mut decoded, false).unwrap();
                    assert_eq!(got, SAMPLES, "{channels}ch frame size");
                    if f >= 4 {
                        for t in 0..SAMPLES {
                            for (c, e) in energy.iter_mut().enumerate() {
                                *e += (decoded[t * ch + c] as f64).powi(2);
                            }
                        }
                    }
                }
                let loudest = (0..ch)
                    .max_by(|&a, &b| energy[a].total_cmp(&energy[b]))
                    .unwrap();
                assert_eq!(
                    loudest, tone_ch,
                    "{channels}ch: tone in channel {tone_ch} must come out on {tone_ch} (energies {energy:?})"
                );
            }
        }
    }

    // ---- A/V sync (audio latency overhaul) ----------------------------------------------

    /// Build an observation whose measured offset is exactly `offset_ms` (positive = audio late).
    fn obs(offset_ms: i64, depth: usize, per_ms: usize) -> AvSyncObservation {
        // audio_e2e = buffered + (now + skew - pts); pin now/skew/pts so the only free term is the
        // buffered depth, then choose video_e2e so the difference lands on `offset_ms`.
        let buffered_ms = (depth / per_ms) as i64;
        let audio_e2e_ms = buffered_ms + 40; // 40 ms of transport, arbitrary but fixed
        let video_e2e_ms = audio_e2e_ms - offset_ms;
        AvSyncObservation {
            pts_ns: 1_000_000_000,
            now_local_ns: 1_000_000_000i128 + 40 * 1_000_000,
            clock_offset_ns: 0,
            buffered_ahead: depth,
            video_e2e_ns: Some((video_e2e_ms.max(0) as u64) * 1_000_000),
        }
    }

    fn settle(sync: &mut AvSync, offset_ms: i64, depth: usize, per_ms: usize, n: u32) {
        for _ in 0..n {
            sync.observe(obs(offset_ms, depth, per_ms));
        }
    }

    #[test]
    fn av_sync_needs_evidence_before_acting() {
        let pm = per_ms(2);
        let mut s = AvSync::new(2);
        // One sample is never enough — the skew estimate and the video figure both settle after
        // connect, and acting on the first would chase the handshake.
        assert!(s.observe(obs(50, 30 * pm, pm)).is_none());
        assert!(!s.settled());
        assert!(s.desired_depth(30 * pm).is_none());
        settle(&mut s, 50, 30 * pm, pm, AV_MIN_OBSERVATIONS);
        assert!(s.settled(), "should act once the evidence is in");
    }

    #[test]
    fn av_sync_aims_shallower_when_audio_is_late() {
        let pm = per_ms(2);
        let depth = 60 * pm;
        let mut s = AvSync::new(2);
        settle(&mut s, 40, depth, pm, AV_MIN_OBSERVATIONS * 4);
        let want = s
            .desired_depth(depth)
            .expect("a 40 ms offset is actionable");
        assert!(
            want < depth,
            "audio late must aim shallower: {want} vs {depth}"
        );
        // The correction is the offset, not a guess at it.
        let shed_ms = (depth - want) / pm;
        assert!(
            (35..=45).contains(&shed_ms),
            "should aim to shed ~40 ms, got {shed_ms}"
        );
    }

    #[test]
    fn av_sync_aims_deeper_when_audio_is_early() {
        let pm = per_ms(2);
        let depth = 20 * pm;
        let mut s = AvSync::new(2);
        settle(&mut s, -30, depth, pm, AV_MIN_OBSERVATIONS * 4);
        let want = s
            .desired_depth(depth)
            .expect("a 30 ms offset is actionable");
        assert!(
            want > depth,
            "audio early must aim deeper: {want} vs {depth}"
        );
    }

    #[test]
    fn av_sync_deadbands_what_no_one_can_hear() {
        let pm = per_ms(2);
        let depth = 30 * pm;
        let mut s = AvSync::new(2);
        settle(
            &mut s,
            (AV_DEADBAND_MS - 2) as i64,
            depth,
            pm,
            AV_MIN_OBSERVATIONS * 4,
        );
        assert!(
            s.desired_depth(depth).is_none(),
            "an offset inside the deadband must not provoke a (real, if crossfaded) discontinuity"
        );
    }

    #[test]
    fn av_sync_rejects_the_implausible_instead_of_clamping_it() {
        let pm = per_ms(2);
        let depth = 30 * pm;
        let mut s = AvSync::new(2);
        settle(&mut s, 30, depth, pm, AV_MIN_OBSERVATIONS * 4);
        let before = s.offset_ms();
        // A wall-clock step / stale video figure. Built directly rather than through `obs`: that
        // helper floors the video figure at zero, which would cap the offset at a merely LARGE
        // value and let this test pass without ever exercising the rejection.
        let wild = AvSyncObservation {
            pts_ns: 0,
            now_local_ns: 5_000_000_000,
            clock_offset_ns: 0,
            buffered_ahead: depth,
            video_e2e_ns: Some(40_000_000),
        };
        assert!(s.observe(wild).is_none());
        assert!(s.implausible(), "a ~5 s offset must be refused, not folded");
        assert_eq!(
            before,
            s.offset_ms(),
            "an implausible sample must be discarded, not folded in"
        );
    }

    #[test]
    fn sync_can_never_starve_the_ring() {
        // THE safety invariant: sync only ever proposes. Continuity — the underrun-driven floor —
        // outranks it on every preset, or a lossy link would be "synced" into dropouts.
        for (name, t) in [
            ("PIPEWIRE", JitterTuning::PIPEWIRE),
            ("WASAPI", JitterTuning::WASAPI),
            ("COREAUDIO", JitterTuning::COREAUDIO),
            ("AAUDIO", JitterTuning::AAUDIO),
        ] {
            let pm = per_ms(2);
            let want = 5 * pm;
            let mut p = JitterPolicy::new(t, 2);
            let floor = p.effective_target(want);
            // Ask for an absurdly shallow ring — zero.
            p.set_sync_target(Some(0));
            assert_eq!(
                p.effective_target(want),
                floor,
                "{name}: sync pulled the target below the continuity floor"
            );
            // And it may not blow past the hard cap either.
            p.set_sync_target(Some(usize::MAX / 2));
            assert!(
                p.effective_target(want) <= t.hard_cap_ms as usize * pm,
                "{name}: sync pushed the target past the hard cap"
            );
        }
    }

    #[test]
    fn a_huge_device_quantum_does_not_panic_the_clamp() {
        // `Ord::clamp` panics when min > max. A device whose callback quantum alone exceeds the
        // preset's hard cap pushes the continuity floor above the ceiling, and this runs inside a
        // realtime audio callback — so the ceiling yields to the floor instead.
        let t = JitterTuning::PIPEWIRE; // hard_cap 80 ms
        let pm = per_ms(2);
        let want = 500 * pm; // a 500 ms quantum: absurd, but not a reason to abort the process
        let mut p = JitterPolicy::new(t, 2);
        p.set_sync_target(Some(0));
        let target = p.effective_target(want); // must not panic
        assert!(
            target >= want,
            "the target must still be able to serve one callback"
        );
    }

    #[test]
    fn no_sync_target_leaves_the_policy_exactly_as_it_was() {
        // The four rings adopt sync one at a time; an un-wired ring must behave bit-identically to
        // before. `None` is the default, so this also pins the constructor.
        let t = JitterTuning::PIPEWIRE;
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut a = JitterPolicy::new(t, 2);
        let mut b = JitterPolicy::new(t, 2);
        b.set_sync_target(None);
        assert_eq!(a.effective_target(want), b.effective_target(want));
        for depth_ms in [0usize, 5, 15, 30, 60, 90, 200] {
            let sa = a.step(depth_ms * pm, want);
            let sb = b.step(depth_ms * pm, want);
            assert_eq!(sa, sb, "depth {depth_ms} ms diverged with an explicit None");
            a.note_read(sa.silence);
            b.note_read(sb.silence);
        }
    }

    #[test]
    fn sync_pressure_relaxes_a_grown_target_sooner_than_time_alone() {
        // A ring that ratcheted during a transient must not hold audio late for minutes after the
        // cause is gone. With sync asking for less, the relax window is the short one.
        let t = JitterTuning::PIPEWIRE;
        let pm = per_ms(2);
        let want = 5 * pm;

        let grow = |p: &mut JitterPolicy| {
            // Drive underruns until the target has grown above the base. Each round hands `step` a
            // DEEP ring first: `note_read` ignores everything while un-primed (a priming silence is
            // not an underrun), and `deprime_after` short reads in a row un-prime the ring — so
            // hammering a zero-depth ring would report nothing and grow nothing, forever.
            for _ in 0..10_000 {
                if p.target_ms() > t.base_target_ms {
                    return;
                }
                p.step(200 * pm, want); // (re-)prime
                p.note_read(true); // then one genuine short read
            }
            panic!("the adaptive floor never grew — the test cannot measure a relax");
        };
        // Quiet reads needed to relax one step, with and without sync pressure.
        let quiet_to_relax = |p: &mut JitterPolicy| -> usize {
            let start = p.target_ms();
            let mut reads = 0usize;
            while p.target_ms() == start && reads < 200_000 {
                p.step(60 * pm, want);
                p.note_read(false);
                reads += 1;
            }
            reads
        };

        let mut slow = JitterPolicy::new(t, 2);
        grow(&mut slow);
        slow.set_sync_target(None);
        let slow_reads = quiet_to_relax(&mut slow);

        let mut fast = JitterPolicy::new(t, 2);
        grow(&mut fast);
        // Ask for something strictly shallower than the grown target.
        fast.set_sync_target(Some(pm));
        let fast_reads = quiet_to_relax(&mut fast);

        assert!(
            fast_reads < slow_reads,
            "sync pressure should relax sooner: {fast_reads} vs {slow_reads} quiet reads"
        );
    }
}
