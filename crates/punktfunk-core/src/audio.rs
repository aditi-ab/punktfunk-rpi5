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
//!
//! Opus is 48 kHz by construction, so the lossless plane is a SECOND plane rather than a
//! parameter change to this one — see [`pcm`] and `design/hi-res-audio.md`.

pub mod pcm;

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
    /// How long the ring may run short before it gives up and goes back to priming, in
    /// MILLISECONDS of starvation — not a count of callbacks.
    ///
    /// It used to be a callback count, and that made the hysteresis mean something different on
    /// every platform, because a callback is not a unit of time: the same `4` was ~40 ms of slack
    /// on a 10 ms WASAPI quantum and **20 ms on iOS**, whose session asks for a 5 ms IO buffer —
    /// the shortest fuse of any client, on the one with the burstiest transport. A 100 ms Wi-Fi
    /// delivery stall then de-primed the Apple ring on every single bunching cycle (measured: 120
    /// audible gaps in 10 minutes at a 5 ms quantum, versus 3 at 8 ms and 1 at 16 ms, on an
    /// otherwise identical link) while the same policy rode it out everywhere else. Expressed in
    /// time, one number means one thing on all four clients and a device's buffer size stops
    /// silently re-tuning the de-prime behaviour.
    ///
    /// A floor of `MIN_DEPRIME_CALLBACKS` callbacks still applies, so a large-quantum device
    /// keeps real hysteresis: `1` reproduces the old `if ring.is_empty() { primed = false }`, where
    /// a single transient drain manufactured a whole target's worth of fresh silence.
    pub deprime_ms: u32,
}

impl JitterTuning {
    /// PipeWire adaptively rate-matches the stream to the graph clock and absorbs a shallow ring,
    /// so Linux can run tight.
    pub const PIPEWIRE: JitterTuning = JitterTuning {
        base_target_ms: 15,
        max_target_ms: 60,
        headroom_ms: 25,
        hard_cap_ms: 80,
        deprime_ms: 40,
    };
    /// WASAPI shared-mode event-driven render: the engine buffers for us, but nothing rate-matches.
    pub const WASAPI: JitterTuning = JitterTuning {
        base_target_ms: 20,
        max_target_ms: 70,
        headroom_ms: 30,
        hard_cap_ms: 90,
        deprime_ms: 50,
    };
    /// CoreAudio via AVAudioEngine — comparable to WASAPI, but the transport is not: this is the
    /// preset an iPad on Wi-Fi runs, so it gets the longer fuse for the same reason [`AAUDIO`]
    /// does. (The old comment here read "the iOS IO buffer is already 5 ms" as grounds for using
    /// WASAPI's callback count unchanged; that quantum is precisely why a count was the wrong unit
    /// — see [`JitterTuning::deprime_ms`].)
    ///
    /// [`AAUDIO`]: JitterTuning::AAUDIO
    pub const COREAUDIO: JitterTuning = JitterTuning {
        base_target_ms: 20,
        max_target_ms: 70,
        headroom_ms: 30,
        hard_cap_ms: 90,
        deprime_ms: 60,
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
        deprime_ms: 60,
    };

    /// How long a packet DROUGHT may be concealed before the ring is allowed to underrun and the
    /// de-prime hysteresis is allowed to run (WP-C1). Twice the de-prime window: long enough to
    /// ride out the delivery stalls that de-prime rings today, short enough that a genuinely dead
    /// stream is not papered over. DERIVED rather than a fifth field, so it cannot drift away from
    /// the fuse it exists to protect — and per-platform for free, since `deprime_ms` already is.
    pub const fn plc_max_ms(&self) -> u32 {
        self.deprime_ms * 2
    }

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

/// A drought must outlast ordinary arrival jitter before anything is synthesized for it: two
/// protocol frames, the same tolerance the host's capture-hole infill uses at the other end.
const DROUGHT_AFTER: std::time::Duration = std::time::Duration::from_millis(2 * FRAME_MS as u64);
/// …and the ring must actually be running out. A drought a deep ring can cover is not audible,
/// and concealing it would synthesize audio the late packets are about to duplicate — pushing the
/// whole stream later and handing the drift shed a mess to clean up audibly.
const DROUGHT_FLOOR_MS: u32 = 2 * FRAME_MS;

/// Bounded concealment of a packet DROUGHT — the client-side twin of the host's capture-hole
/// infill (design/host-source-stutter-fixes.md, WP-C1).
///
/// The decode path already conceals a SEQ GAP: [`AudioGapTracker`] reports the packets missing
/// before the one that arrived and libopus synthesizes each from the decoder's own state. But that
/// only fires when a LATER packet arrives to reveal the gap. When the wire simply goes quiet — a
/// delivery stall on a bunching Wi-Fi link, or a host whose capture stalled — nothing arrives to
/// reveal anything: the ring drains to empty, the callback runs short, and
/// [`JitterPolicy::note_read`] de-primes and then re-primes a whole target's worth of fresh
/// silence. The artifact is far longer than the audio actually missing, and this is the shape the
/// 2026-08-15 field session spent 3–16 % of its wall-clock in.
///
/// So a drought that is draining the ring gets concealed too, from the same decoder state, for a
/// bounded time. Denominated in TIME, never in frames or callbacks: that is the recorded lesson
/// from the very fuse this protects, where a count gave an iPad a third of a Mac's slack for no
/// reason anyone intended.
///
/// Time is passed IN, so the policy stays as syscall-free and deterministic as the rest of this
/// module.
pub struct DroughtConceal {
    /// Concealed since the last real packet.
    concealed_ms: u32,
    max_ms: u32,
    /// Concealed over the session — what the 10 s `plc_ms=` line reports. Concealment must be
    /// visible: a policy that quietly papers over a failing link is a policy that hides the bug.
    total_ms: u64,
}

impl DroughtConceal {
    pub fn new(max_ms: u32) -> DroughtConceal {
        DroughtConceal {
            concealed_ms: 0,
            max_ms,
            total_ms: 0,
        }
    }

    /// A packet arrived, ending any drought. Returns how many FRAMES were concealed for it, so the
    /// caller can subtract them from the loss concealment [`AudioGapTracker`] is about to ask for:
    /// packets genuinely lost inside a drought we already covered must not be covered twice, which
    /// would insert audio the stream never had and push everything after it later.
    pub fn packet(&mut self) -> u32 {
        std::mem::take(&mut self.concealed_ms) / FRAME_MS
    }

    /// Should one more frame be concealed? `depth_ms` is the playout ring as the callback last
    /// saw it.
    pub fn conceal(&mut self, since_last_packet: std::time::Duration, depth_ms: u32) -> bool {
        if since_last_packet < DROUGHT_AFTER
            || depth_ms > DROUGHT_FLOOR_MS
            || self.concealed_ms >= self.max_ms
        {
            return false;
        }
        self.concealed_ms += FRAME_MS;
        self.total_ms += FRAME_MS as u64;
        true
    }

    /// Concealment over the session, ms.
    pub fn total_ms(&self) -> u64 {
        self.total_ms
    }
}

/// What one callback should do, from [`JitterPolicy::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct JitterStep {
    /// Interleaved samples to discard from the FRONT of the ring before reading.
    pub drop_front: usize,
    /// Interleaved samples of linear crossfade to apply across the seam left by `drop_front`
    /// ([`crossfade_drop`] does it for a `VecDeque<f32>` ring). Zero only when nothing is dropped.
    ///
    /// BOTH kinds of drop are faded. The hard-cap trim used to splice raw, on the reasoning that a
    /// ring which blew its ceiling "is already a discontinuity" — but that is a statement about the
    /// ARRIVALS, not about the samples either side of the seam, which are ordinary continuous
    /// audio. It is also the drop that actually fires in the field: a bunching Wi-Fi link trimmed
    /// 120 times in 10 simulated minutes where the smooth shed fired for drift a handful of times.
    /// The gentle path that almost never runs was the one being faded.
    pub crossfade: usize,
    /// `drop_front` was the hard-cap backstop (a burst blew the ceiling) rather than the smooth
    /// drift shed. Both fade now, so the fade length no longer distinguishes them — and the two
    /// mean very different things to anyone reading logs or a test: sheds are the policy working,
    /// trims are the link outrunning the headroom.
    pub hard_trim: bool,
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
/// Post-read depth below which a served callback counts as a NEAR-MISS: the device got its
/// samples, but less than one protocol frame was left in hand, so the next callback starves
/// unless a packet lands inside one frame time. On a healthy link the post-read depth hovers a
/// whole target above this, which is what makes a near-miss evidence of real delivery jitter —
/// the same evidence as an underrun, except nobody heard it yet.
const NEAR_MISS_MARGIN_MS: u32 = FRAME_MS;
/// How long a shrink remains a PROBE, in consumed audio: an underrun or near-miss inside this
/// window means the shrink was wrong, and the previous target is restored at once instead of
/// being re-learned three audible underruns at a time.
const SHRINK_PROBE_MS: u32 = 5_000;
/// A ring is HOLLOW when its depth AVERAGE sits this far below the target: the target promises a
/// depth the ring does not actually hold. Growth only ever raises the promise — the one thing
/// that re-banks real depth is a re-prime — so an underrun in a hollow ring re-primes AT ONCE:
/// the click has already happened, and spending it on the whole refill is strictly better than
/// riding the knife edge and paying a click per bunching period indefinitely, which is what the
/// consecutive-empties hysteresis alone converges to. A full ring's underrun (one packet a few
/// ms late) is nowhere near hollow and keeps the hysteresis.
const DEPRIME_DEBT_MS: u32 = GROW_STEP_MS;
/// Floor, in callbacks, under `JitterTuning::deprime_ms`: however short the starvation window works
/// out to in time, a de-prime always needs at least this many consecutive short reads. A device
/// with a quantum at or above `deprime_ms` would otherwise de-prime on the FIRST short read —
/// exactly the "a single transient drain manufactures a whole target of fresh silence" defect the
/// hysteresis exists to prevent, reintroduced at the other end of the quantum range.
///
/// Deliberately NOT `pub`: it is an internal detail of the policy, and cbindgen exports every
/// public const into the C header, where this one would land unprefixed next to
/// `PUNKTFUNK_AUDIO_*` and pollute every embedder's macro namespace.
const MIN_DEPRIME_CALLBACKS: u32 = 2;
// A de-prime on the FIRST short read is the defect the hysteresis exists to prevent, so hold the
// floor at build time rather than in a test: tuning it to 1 should not compile.
const _: () = assert!(MIN_DEPRIME_CALLBACKS >= 2);
/// How long a failed probe keeps the sync loop from driving another shrink. Without this the
/// loop pays an audible starvation event every [`SHRINK_QUIET_SYNC_MS`] on any link whose jitter
/// genuinely needs the depth — sync asks for less, the ring shrinks, the link answers, the ring
/// grows back, five quiet seconds later sync asks again, forever. Doubles per consecutive
/// failure up to [`SYNC_BACKOFF_MAX_MS`]; a probe that survives its window resets it.
const SYNC_BACKOFF_MS: u32 = 60_000;
const SYNC_BACKOFF_MAX_MS: u32 = 480_000;

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
    /// Interleaved samples per millisecond at the negotiated layout (`rate_hz / 1000 × channels`
    /// — 48 × channels at the default rate, 96 × channels for a 96 kHz hi-res session).
    per_ms: usize,
    /// The live target, in interleaved samples — `base_target_ms` grown by underrun pressure.
    target: usize,
    primed: bool,
    /// Consecutive short reads, and the audio they starved for in interleaved samples. BOTH gate
    /// the de-prime: the run must be at least [`JitterTuning::deprime_ms`] long AND at least
    /// [`MIN_DEPRIME_CALLBACKS`] callbacks, so the hysteresis means the same span of time whatever
    /// the device's quantum, without collapsing to a hair trigger on a large-quantum device.
    empties: u32,
    empties_run: usize,
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
    /// Set by [`step`](Self::step) when the read it authorised leaves less than
    /// [`NEAR_MISS_MARGIN_MS`] buffered; consumed by [`note_read`](Self::note_read).
    near_miss: bool,
    /// A near-miss already grew the target this window — one step per window, so a single
    /// bunching episode (which lands as a RUN of consecutive near-misses while the ring refills)
    /// buys one measured step, not a sprint to the ceiling.
    near_miss_grown: bool,
    /// Set by [`step`](Self::step): the depth average sits more than [`DEPRIME_DEBT_MS`] below
    /// the target, so an underrun should re-prime at once instead of waiting out the hysteresis.
    hollow: bool,
    /// Consumed samples left in the current shrink-probe window (0 = no probe outstanding).
    probe_run: usize,
    /// The live target before the probed shrink, restored if the probe fails.
    probe_prev_target: usize,
    /// Consumed samples before the sync loop may drive another shrink (0 = allowed now).
    sync_backoff_run: usize,
    /// Length of the NEXT backoff, in ms — doubles per consecutive failed probe, capped.
    sync_backoff_ms: u32,
}

impl JitterPolicy {
    /// `channels` is the negotiated interleaved channel count (2/6/8), at the protocol's default
    /// [`SAMPLE_RATE_HZ`]. Hi-res sessions use [`new_at_rate`](Self::new_at_rate).
    pub fn new(tuning: JitterTuning, channels: u8) -> JitterPolicy {
        Self::new_at_rate(tuning, channels, SAMPLE_RATE_HZ)
    }

    /// As [`new`](Self::new), at an explicitly negotiated `rate_hz`.
    ///
    /// **`rate_hz` must be a whole number of samples per millisecond**, and every figure this
    /// type computes — target, EWMA depth, shed threshold, hard cap, the de-prime fuse, and the
    /// `buffer_ms`/`target_ms` a client reports — is `ms * per_ms` with `per_ms` an *integer*.
    /// 48 000 → 48 and 96 000 → 96 are exact; **44 100 → 44.1 truncates to 44**, a silent 2.3 %
    /// error in all of them. That is why the hi-res ladder is 48/96 kHz only: 44.1/88.2/176.4
    /// are deferred behind denominating this policy in a rational `per_ms`, not behind any
    /// difficulty in carrying them on the wire (`design/hi-res-audio.md` §4.1).
    ///
    /// The debug assertion below is that deferral's tripwire — it fires the moment someone adds
    /// a rate the arithmetic cannot represent, rather than letting the error hide in a buffer
    /// figure that merely looks 2 % optimistic. The tell in a release build is
    /// [`depth_ms`](Self::depth_ms)`(target)` not round-tripping to [`target_ms`](Self::target_ms).
    pub fn new_at_rate(tuning: JitterTuning, channels: u8, rate_hz: u32) -> JitterPolicy {
        debug_assert_eq!(
            rate_hz % 1000,
            0,
            "JitterPolicy is denominated in integer samples per millisecond; {rate_hz} Hz \
             truncates and would skew every depth/target figure (see design/hi-res-audio.md §4.1)"
        );
        let per_ms = (rate_hz / 1000) as usize * channels.max(1) as usize;
        JitterPolicy {
            tuning,
            per_ms,
            target: tuning.base_target_ms as usize * per_ms,
            primed: false,
            empties: 0,
            empties_run: 0,
            depth_avg: 0.0,
            over_run: 0,
            underruns: 0,
            window_run: 0,
            quiet_run: 0,
            last_want: 0,
            sync_target: None,
            near_miss: false,
            near_miss_grown: false,
            hollow: false,
            probe_run: 0,
            probe_prev_target: 0,
            sync_backoff_run: 0,
            sync_backoff_ms: SYNC_BACKOFF_MS,
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
            // Blew the ceiling: a burst arrived, or we were wedged. Discard down to the cap and
            // reset the drift timer so the trim isn't double-counted as drift. Faded like any
            // other drop — see `JitterStep::crossfade` for why this used to splice raw and why
            // that was backwards.
            out.drop_front = depth - cap;
            out.hard_trim = true;
            out.crossfade = (SHED_CROSSFADE_MS as usize * self.per_ms)
                .min(depth.saturating_sub(out.drop_front));
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
            self.empties_run = 0;
            // The refill just banked this much: seed the average with it rather than letting it
            // climb from wherever the drought left it — a freshly-primed ring would otherwise
            // read as hollow for the EWMA's whole settling time, and the FIRST late packet
            // would re-prime a ring that is actually full.
            self.depth_avg = depth.saturating_sub(out.drop_front) as f32;
        }
        out.silence = !self.primed;
        // Near-miss: this read will be served, but with less than one frame left over — the
        // next callback starves unless a packet lands within one frame time. Unconditional
        // assignment, so a stale flag can never survive a de-prime into the next primed read.
        let after = depth.saturating_sub(out.drop_front);
        self.near_miss = self.primed
            && after >= want
            && after - want < NEAR_MISS_MARGIN_MS as usize * self.per_ms;
        // Hollow: the depth AVERAGE runs a debt against the target — the promise has been raised
        // but the depth was never re-banked (see `DEPRIME_DEBT_MS`). Judged on the average, not
        // this instant: a single late packet empties the ring for a callback without making it
        // hollow, and must keep the consecutive-empties hysteresis.
        self.hollow = self.primed
            && (self.depth_avg as usize + DEPRIME_DEBT_MS as usize * self.per_ms) < target;
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
        let near_miss = std::mem::take(&mut self.near_miss);
        self.window_run += want;
        if self.window_run >= GROW_WINDOW_MS as usize * self.per_ms {
            self.window_run = 0;
            self.underruns = 0;
            self.near_miss_grown = false;
        }
        self.sync_backoff_run = self.sync_backoff_run.saturating_sub(want);
        let mut restored = false;
        if self.probe_run > 0 {
            self.probe_run = self.probe_run.saturating_sub(want);
            if ran_short || near_miss {
                // The probe FAILED: the link answered a shrink with (nearly) starving the ring.
                // Take the depth straight back — re-learning it three audible underruns at a
                // time is what made the sync-vs-growth tug-of-war audible — and keep the sync
                // loop from probing again for a while, doubling per consecutive failure. The
                // residual A/V offset is reported instead; continuity outranks sync. The
                // restore CONSUMES this event as growth evidence: it answered a depth the ring
                // is no longer at, so growing past the proven target on top would overshoot.
                self.probe_run = 0;
                self.target = self.target.max(self.probe_prev_target);
                self.sync_backoff_run = self.sync_backoff_ms as usize * self.per_ms;
                self.sync_backoff_ms = (self.sync_backoff_ms * 2).min(SYNC_BACKOFF_MAX_MS);
                restored = true;
            } else if self.probe_run == 0 {
                // Survived the whole window: the shallower depth is genuinely safe here, so the
                // next probe starts from a clean slate.
                self.sync_backoff_ms = SYNC_BACKOFF_MS;
            }
        }
        if ran_short {
            self.quiet_run = 0;
            self.empties += 1;
            self.empties_run += want;
            // Starved for `deprime_ms` of audio, over at least MIN_DEPRIME_CALLBACKS callbacks.
            // Both, because either alone is wrong at one end of the quantum range: time alone is a
            // hair trigger on a device whose single quantum already exceeds the window, and a
            // callback count alone is the platform-dependent fuse this replaced.
            let starved = self.empties_run >= self.tuning.deprime_ms as usize * self.per_ms
                && self.empties >= MIN_DEPRIME_CALLBACKS;
            if starved || self.hollow {
                // The starvation hysteresis protects a FULL ring from one late packet. A hollow
                // ring is the opposite case: the target has been raised but the depth never
                // re-banked (growth is a promise; only a re-prime cashes it), and riding that out
                // is a click per bunching period, forever. The click just heard has already paid
                // for the refill — take it now.
                self.primed = false;
                self.empties = 0;
                self.empties_run = 0;
            }
            if !restored {
                self.underruns += 1;
            }
            if self.underruns >= GROW_UNDERRUNS {
                // This device genuinely needs more slack than the base target. Grow ONCE per
                // window, capped — the alternative (every device pre-paying the worst device's
                // depth) is what the fixed 40 ms Android floor was.
                self.underruns = 0;
                self.window_run = 0;
                let grown = self.target + GROW_STEP_MS as usize * self.per_ms;
                self.target = grown.min(self.tuning.max_target_ms as usize * self.per_ms);
            }
        } else if near_miss {
            // Came within one frame of an underrun — the same evidence as one, heard by no one.
            // Growing here, BEFORE the click, is what "no audible jitter" means: waiting for
            // the third audible underrun means the user heard two. One step per window (a
            // bunching episode is a RUN of near-misses while the ring refills, and must buy one
            // measured step, not a sprint to the ceiling); if it worsens into real underruns
            // the path above takes over. A near-miss is pressure, not quiet.
            self.quiet_run = 0;
            self.empties = 0;
            self.empties_run = 0;
            if !self.near_miss_grown && !restored {
                self.near_miss_grown = true;
                let grown = self.target + GROW_STEP_MS as usize * self.per_ms;
                self.target = grown.min(self.tuning.max_target_ms as usize * self.per_ms);
            }
        } else {
            self.empties = 0;
            self.empties_run = 0;
            self.quiet_run += want;
            // A grown target normally relaxes only after a long quiet spell, because without other
            // evidence the only thing that can justify giving up hard-won slack is time. When the
            // sync loop is asking to run shallower it IS that evidence — a measurement saying the
            // extra depth is costing alignment right now — so test a smaller target sooner. Every
            // shrink is armed as a PROBE: answered by an underrun or near-miss it is undone at
            // once (see above), and a failed sync-driven guess is not retried for a backoff —
            // without that, a link whose jitter genuinely needs the depth pays an audible
            // starvation event every five seconds, forever.
            let sync_shrink = self.sync_wants_less() && self.sync_backoff_run == 0;
            let quiet_needed = if sync_shrink {
                SHRINK_QUIET_SYNC_MS
            } else {
                SHRINK_QUIET_MS
            };
            if self.quiet_run >= quiet_needed as usize * self.per_ms {
                // Long quiet spell: give a grown target one step back, so a single bad minute
                // doesn't cost latency for the rest of the session.
                self.quiet_run = 0;
                let base = self.tuning.base_target_ms as usize * self.per_ms;
                let prev = self.target;
                self.target = self
                    .target
                    .saturating_sub(GROW_STEP_MS as usize * self.per_ms)
                    .max(base);
                if self.target < prev {
                    self.probe_run = SHRINK_PROBE_MS as usize * self.per_ms;
                    self.probe_prev_target = prev;
                }
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
/// continuous across the splice. `fade == 0` discards hard; no caller in the policy asks for that
/// any more (see [`JitterStep::crossfade`]), but it stays honoured for callers that splice at a
/// point they know is already discontinuous. Shared by the three `VecDeque<f32>` rings; the Apple
/// ring is index-based and mirrors this in Swift.
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
    //
    // Blended in place and BEFORE the drain, with no scratch buffer: a value written at `drop + i`
    // can never be read again as a fade-OUT source, because those sources are `drop - fade + j` for
    // `j < fade`, i.e. strictly below `drop`. One ascending pass is therefore safe — and this runs
    // inside realtime audio callbacks, where the `Vec` this used to allocate had no business being.
    // It now runs on every hard-cap trim too, which is the common case on a bunching link.
    for i in 0..fade {
        let old = ring[drop - fade + i];
        let new = ring[drop + i];
        let t = (i + 1) as f32 / (fade + 1) as f32;
        ring[drop + i] = old * (1.0 - t) + new * t;
    }
    ring.drain(..drop);
}

/// Where [`apply_gain`]'s soft knee begins, in linear amplitude (≈ −3.1 dBFS). Below this the
/// gained signal is passed through EXACTLY — a boost whose peaks never reach the knee is plain
/// multiplication, sample for sample, so the limiter costs nothing on material that does not need
/// it.
pub const SOFT_LIMIT_KNEE: f32 = 0.7;

/// Multiply `samples` by `gain`, bending anything that would overshoot full scale into a soft knee
/// instead of slicing it flat.
///
/// **Why this is not a `clamp`.** The GameStream plane's gain was `(s * gain).clamp(-1.0, 1.0)`,
/// which is a hard clip: the waveform's peaks are replaced by literal flat tops, and a flat top is
/// a discontinuity in the first derivative. That radiates high-order harmonics — the harsher and
/// more aliasing-prone the higher they go — which is why a field report of "+18 dB and everything
/// warbles" is the expected outcome of that code and not a bug in anything downstream. Any operator
/// who set `PUNKTFUNK_AUDIO_GAIN` much above ~1.5 was hearing this.
///
/// The curve here is `tanh`-based and chosen for three properties, in this order:
///
/// 1. **C¹-continuous at the knee.** The shaped branch's slope at `m == KNEE` is
///    `(1-K) · sech²(0) · 1/(1-K) == 1`, exactly the slope of the linear branch it meets. There is
///    no corner in the transfer curve, so the onset of limiting is not itself an audible event —
///    the failure mode of a naïve piecewise limiter, which trades one discontinuity for another.
/// 2. **Bounded by construction.** `tanh` is asymptotic to 1, so the output approaches but never
///    exceeds full scale for any finite input, and `±inf` maps to `±1.0`. No sample can leave here
///    out of range, which is what the encoder downstream assumes.
/// 3. **Odd-symmetric.** `f(-x) == -f(x)`, so the distortion it does introduce is odd-harmonic and
///    adds no DC offset — the benign, "saturating" flavour rather than the rectifying one.
///
/// Callers gate on `gain != 1.0`, so the default path is untouched and the wire stays byte-for-byte
/// identical to a build without this. Note this is a WAVESHAPER, not a lookahead limiter: it is
/// memoryless and therefore costs zero latency, which is the trade that makes it acceptable in the
/// realtime encode path. It raises headroom; it does not raise *loudness* the way a compressor
/// with a real time constant would, and it should not be sold as one.
pub fn apply_gain(samples: &mut [f32], gain: f32) {
    // Unity is a no-op, not "multiply by one and shape": the shaper is only correct to apply to a
    // signal somebody asked to boost. Without this, calling at unity would bend every peak above
    // the knee — a silent quality change for anyone who forgot to gate the call, and the reason
    // the callers' `gain != 1.0` guards are a convenience rather than a load-bearing contract.
    if gain == 1.0 {
        return;
    }
    for s in samples {
        *s = soft_limit(*s * gain);
    }
}

/// The waveshaper behind [`apply_gain`]: identity below [`SOFT_LIMIT_KNEE`], asymptotic to ±1.0
/// above it. Exposed so the clients can mirror the curve if they ever grow a gain of their own.
pub fn soft_limit(x: f32) -> f32 {
    let m = x.abs();
    if m <= SOFT_LIMIT_KNEE {
        return x;
    }
    let head = 1.0 - SOFT_LIMIT_KNEE;
    let shaped = SOFT_LIMIT_KNEE + head * ((m - SOFT_LIMIT_KNEE) / head).tanh();
    if x < 0.0 {
        -shaped
    } else {
        shaped
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
    /// Concealment the decode side has synthesized this session, ms — telemetry travelling the
    /// same way the target does. It rides here because the counter is produced on the decode
    /// thread and the 10 s playback line is emitted from the callback, and concealment that
    /// nobody can see is concealment that hides the bug it is covering (WP-C1, risk R6).
    plc_ms: std::sync::atomic::AtomicU64,
}

impl Default for AudioSyncCell {
    fn default() -> Self {
        AudioSyncCell {
            depth: std::sync::atomic::AtomicUsize::new(0),
            target: std::sync::atomic::AtomicUsize::new(usize::MAX),
            plc_ms: std::sync::atomic::AtomicU64::new(0),
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

    /// Decode side: publish total concealment synthesized for packet droughts.
    pub fn publish_plc_ms(&self, ms: u64) {
        self.plc_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Callback side: that total, for the periodic playback line.
    pub fn plc_ms(&self) -> u64 {
        self.plc_ms.load(std::sync::atomic::Ordering::Relaxed)
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
    /// Interleaved samples per millisecond at the negotiated layout (`rate_hz / 1000 × channels`).
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
    /// `channels` is the negotiated interleaved channel count (2/6/8), at the protocol's default
    /// [`SAMPLE_RATE_HZ`]. Hi-res sessions use [`new_at_rate`](Self::new_at_rate).
    pub fn new(channels: u8) -> AvSync {
        Self::new_at_rate(channels, SAMPLE_RATE_HZ)
    }

    /// As [`new`](Self::new), at an explicitly negotiated `rate_hz`. The same integer
    /// samples-per-millisecond constraint as [`JitterPolicy::new_at_rate`] applies, and for the
    /// same reason — this type's proposal is denominated in the ring's own units so the two
    /// agree about what a millisecond is.
    pub fn new_at_rate(channels: u8, rate_hz: u32) -> AvSync {
        debug_assert_eq!(
            rate_hz % 1000,
            0,
            "AvSync is denominated in integer samples per millisecond; {rate_hz} Hz truncates \
             (see design/hi-res-audio.md §4.1)"
        );
        AvSync {
            per_ms: (rate_hz / 1000) as usize * channels.max(1) as usize,
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

    // ---- drought concealment (WP-C1) -----------------------------------------------------

    /// Concealment is for a ring that is running OUT. A drought a deep ring can cover is
    /// inaudible, and synthesizing over it would insert audio the late packets are about to
    /// duplicate — the stream would then run permanently later and the drift shed would have to
    /// cut it back out, audibly.
    #[test]
    fn a_drought_is_concealed_only_while_the_ring_is_running_out() {
        let mut c = DroughtConceal::new(JitterTuning::PIPEWIRE.plc_max_ms());
        let stalled = DROUGHT_AFTER + std::time::Duration::from_millis(FRAME_MS as u64);
        assert!(
            !c.conceal(stalled, 40),
            "a 40 ms ring covers this drought by itself"
        );
        assert!(c.conceal(stalled, 0), "an empty ring does not");
        assert_eq!(c.total_ms(), FRAME_MS as u64);
    }

    /// Ordinary arrival jitter is not a drought — this policy must be invisible until the wire
    /// has genuinely stopped.
    #[test]
    fn ordinary_jitter_is_not_a_drought() {
        let mut c = DroughtConceal::new(JitterTuning::AAUDIO.plc_max_ms());
        for _ in 0..1_000 {
            assert!(!c.conceal(std::time::Duration::from_millis(FRAME_MS as u64), 0));
        }
        assert_eq!(c.total_ms(), 0);
        assert_eq!(c.packet(), 0);
    }

    /// The window is bounded, and bounded in TIME — the whole reason `deprime_ms` stopped being a
    /// callback count. Every preset must get exactly twice its own de-prime fuse, so no platform
    /// silently gets a third of another's protection again.
    #[test]
    fn drought_concealment_is_bounded_at_twice_the_deprime_fuse() {
        for t in [
            JitterTuning::PIPEWIRE,
            JitterTuning::WASAPI,
            JitterTuning::COREAUDIO,
            JitterTuning::AAUDIO,
        ] {
            assert_eq!(t.plc_max_ms(), t.deprime_ms * 2);
            let mut c = DroughtConceal::new(t.plc_max_ms());
            let mut ms = 0u32;
            while c.conceal(DROUGHT_AFTER, 0) {
                ms += FRAME_MS;
                assert!(ms <= t.plc_max_ms(), "ran past the budget for {t:?}");
            }
            assert_eq!(ms, t.plc_max_ms(), "must use exactly the budget for {t:?}");
        }
    }

    /// Packets genuinely lost INSIDE a drought we already covered must not be covered a second
    /// time by the loss path: doing both would insert audio the stream never carried and push
    /// everything after it later.
    #[test]
    fn concealment_already_paid_for_is_not_paid_for_twice() {
        let mut c = DroughtConceal::new(JitterTuning::WASAPI.plc_max_ms());
        for _ in 0..4 {
            assert!(c.conceal(DROUGHT_AFTER, 0));
        }
        let mut gaps = AudioGapTracker::new();
        gaps.missing_before(10);
        // Four frames concealed; the wire then reveals six were lost. Only two are still owed.
        let already = c.packet();
        assert_eq!(already, 4);
        assert_eq!(gaps.missing_before(17).saturating_sub(already), 2);
        // …and the next drought starts from a full budget.
        assert!(c.conceal(DROUGHT_AFTER, 0));
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
                // Told apart by `hard_trim`, not by the fade length — both kinds fade now.
                if s.hard_trim {
                    out.hard_trims += 1;
                } else {
                    out.soft_sheds += 1;
                }
                assert!(
                    s.crossfade > 0,
                    "every drop must be faded: dropped {} with no crossfade",
                    s.drop_front
                );
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
            // Real hysteresis, in time: a drought has to outlast several protocol frames before
            // the ring gives up, or one late packet manufactures a whole target of fresh silence.
            assert!(
                t.deprime_ms >= 4 * FRAME_MS,
                "{name}: de-primes after {} ms — a single late packet would trip it",
                t.deprime_ms
            );
            // ...and never longer than the deepest buffer this preset would ever hold: past that
            // point the drought has already cost more than the re-prime it is trying to avoid, and
            // every callback in between is dribbling partial reads at the listener.
            assert!(
                t.deprime_ms <= t.max_target_ms,
                "{name}: waits {} ms to de-prime but never buffers more than {} ms",
                t.deprime_ms,
                t.max_target_ms
            );
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
        assert!(s.hard_trim, "a cap trim must announce itself as one");
        // ...and it is FADED. This used to assert the opposite ("a blown cap is already a
        // discontinuity"), which confused the arrivals with the audio: the samples either side of
        // the splice are ordinary continuous sound, and a raw seam through them is a click. It is
        // also the drop that actually fires in the field — a bunching Wi-Fi link trims far more
        // often than drift sheds — so the one path that was left unfaded was the audible one.
        assert!(
            s.crossfade > 0,
            "a cap trim splices real audio and must be faded"
        );
        assert!(
            s.crossfade <= s.drop_front,
            "the fade cannot outrun what is being dropped"
        );
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
        let deprime = JitterTuning::PIPEWIRE.deprime_ms as usize;
        for _ in 1..(deprime / 5) {
            p.note_read(true);
        }
        assert!(!p.is_primed(), "a sustained drain must re-prime");
    }

    /// THE regression this replaced a callback count for: the de-prime fuse must be the same
    /// SPAN OF TIME whatever the device's IO quantum. As a count it was not — the same `4` was
    /// ~40 ms on a 10 ms WASAPI quantum and 20 ms on iOS, whose session asks for a 5 ms IO buffer.
    /// A Wi-Fi delivery stall therefore de-primed the Apple ring on every bunching cycle while the
    /// identical policy rode it out everywhere else. Plant the defect by restoring a fixed count
    /// and the two quanta below stop agreeing.
    #[test]
    fn deprime_fuse_is_a_duration_not_a_callback_count() {
        for quantum_ms in [5usize, 8, 10, 16, 21] {
            let t = JitterTuning::COREAUDIO;
            let pm = per_ms(2);
            let want = quantum_ms * pm;
            let mut p = JitterPolicy::new(t, 2);
            // Prime well above target so the hysteresis path is what we measure, not `hollow`.
            assert!(!p.step(80 * pm, want).silence);
            assert!(p.is_primed());
            let mut starved_ms = 0;
            while p.is_primed() && starved_ms < 10 * t.deprime_ms as usize {
                p.note_read(true);
                starved_ms += quantum_ms;
            }
            assert!(!p.is_primed(), "q={quantum_ms}ms: never de-primed at all");
            // One quantum of granularity either side — the fuse can only be checked per callback.
            let floor = (t.deprime_ms as usize).min(quantum_ms * MIN_DEPRIME_CALLBACKS as usize);
            assert!(
                starved_ms >= floor && starved_ms < t.deprime_ms as usize + quantum_ms,
                "q={quantum_ms}ms de-primed after {starved_ms} ms, not ~{} ms — the fuse is still \
                 scaling with the quantum",
                t.deprime_ms
            );
        }
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

    /// The rate parameter must move SAMPLES without moving MILLISECONDS. A 96 kHz ring holds
    /// twice the samples for the same latency, and every ms-denominated figure a client reports
    /// — `target_ms`, `depth_ms`, `avg_depth_ms` — must read identically at both rates. If this
    /// ever fails, the hi-res plane is buying latency it did not intend to.
    #[test]
    fn a_hires_policy_holds_the_same_milliseconds_as_a_48k_one() {
        for t in [
            JitterTuning::PIPEWIRE,
            JitterTuning::WASAPI,
            JitterTuning::COREAUDIO,
            JitterTuning::AAUDIO,
        ] {
            for ch in [2u8, 6, 8] {
                let lo = JitterPolicy::new_at_rate(t, ch, 48_000);
                let hi = JitterPolicy::new_at_rate(t, ch, 96_000);
                assert_eq!(
                    lo.target_ms(),
                    hi.target_ms(),
                    "96 kHz must start at the same latency as 48 kHz ({t:?}, {ch}ch)"
                );
                // …and the sample counts behind those milliseconds really did double.
                assert_eq!(
                    hi.depth_ms(2 * lo.per_ms),
                    lo.depth_ms(lo.per_ms),
                    "a 96 kHz ring needs twice the samples for the same ms ({t:?}, {ch}ch)"
                );
                assert_eq!(hi.per_ms, 2 * lo.per_ms, "per_ms must scale with the rate");
            }
        }
    }

    /// `new` is exactly `new_at_rate` at the protocol default — the property that let every
    /// existing caller and all 17 policy tests keep their behaviour when the rate became a
    /// parameter.
    #[test]
    fn the_default_constructor_is_the_default_rate() {
        let a = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
        let b = JitterPolicy::new_at_rate(JitterTuning::PIPEWIRE, 2, SAMPLE_RATE_HZ);
        assert_eq!(a.per_ms, b.per_ms);
        assert_eq!(a.target, b.target);
        assert_eq!(a.target_ms(), b.target_ms());
        let x = AvSync::new(2);
        let y = AvSync::new_at_rate(2, SAMPLE_RATE_HZ);
        assert_eq!(x.per_ms, y.per_ms);
    }

    /// §4.1's tripwire, as an assertion rather than a comment: on the shipping ladder the
    /// ms↔sample conversion round-trips exactly. This is the test that would fail first if
    /// someone added 44 100 Hz without denominating the policy in a rational `per_ms` — at
    /// 44.1 samples/ms `per_ms` truncates to 44 and every figure below drifts 2.3 % low.
    #[test]
    fn the_shipping_rate_ladder_round_trips_ms_to_samples_exactly() {
        for rate in [48_000u32, 96_000] {
            for ch in [2u8, 6, 8] {
                assert_eq!(
                    rate as usize / 1000 * ch as usize,
                    JitterPolicy::new_at_rate(JitterTuning::PIPEWIRE, ch, rate).per_ms,
                    "{rate} Hz × {ch}ch must be a whole number of samples per ms"
                );
                let p = JitterPolicy::new_at_rate(JitterTuning::PIPEWIRE, ch, rate);
                assert_eq!(
                    p.depth_ms(p.target),
                    p.target_ms(),
                    "depth_ms(target) must round-trip to target_ms at {rate} Hz"
                );
            }
        }
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

    // ---- near-miss growth and shrink probes (the audible-limit-cycle fixes) ---------------

    /// A primed read that is served but leaves less than one frame buffered is a NEAR-MISS —
    /// the same evidence as an underrun, heard by no one — and must grow the target BEFORE the
    /// click, not after the third one. One step per window: a bunching episode lands as a run of
    /// consecutive near-misses while the ring refills, and must not sprint to the ceiling.
    #[test]
    fn a_near_miss_grows_the_target_without_an_underrun() {
        let t = JitterTuning::COREAUDIO;
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(t, 2);
        p.step(t.base_target_ms as usize * pm, want); // primes exactly at target
        assert!(p.is_primed());
        let base = p.target_ms();
        // Serve the callback with less than one frame left over: depth = want + (margin − 1).
        p.step(want + NEAR_MISS_MARGIN_MS as usize * pm - 1, want);
        p.note_read(false); // NOT short — the device got its samples
        assert_eq!(
            p.target_ms(),
            base + GROW_STEP_MS,
            "a near-miss must buy one step"
        );
        // A second near-miss in the same window is the same episode: no further growth.
        p.step(want + pm, want);
        p.note_read(false);
        assert_eq!(p.target_ms(), base + GROW_STEP_MS, "one step per window");
        // A healthy read does not grow anything.
        let grown = p.target_ms();
        p.step(grown as usize * pm + want, want);
        p.note_read(false);
        assert_eq!(p.target_ms(), grown);
    }

    /// A healthy steady depth must never read as a near-miss: the margin is one frame, and a
    /// ring hovering at target sits a whole target above it.
    #[test]
    fn steady_depth_never_grows_the_target() {
        let t = JitterTuning::PIPEWIRE;
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(t, 2);
        for _ in 0..(60_000 / 5) {
            // one minute of clean callbacks
            p.step(t.base_target_ms as usize * pm + want, want);
            p.note_read(false);
        }
        assert_eq!(p.target_ms(), t.base_target_ms);
    }

    /// A shrink answered by an underrun (or near-miss) inside its probe window is undone AT
    /// ONCE — re-learning the depth three audible underruns at a time is what made the
    /// sync-vs-growth tug-of-war audible in the field.
    #[test]
    fn a_failed_shrink_probe_is_undone_at_once() {
        let t = JitterTuning::COREAUDIO;
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(t, 2);
        // Grow the floor two steps the audible way.
        for _ in 0..(2 * GROW_UNDERRUNS) {
            while !p.is_primed() {
                p.step(200 * pm, want);
            }
            p.step(200 * pm, want);
            p.note_read(true);
        }
        let grown = p.target_ms();
        assert!(grown > t.base_target_ms);
        // Sync asks for less; five quiet seconds later the shrink probes.
        p.set_sync_target(Some(pm));
        let depth = grown as usize * pm + want;
        while p.target_ms() == grown {
            p.step(depth, want);
            p.note_read(false);
        }
        assert_eq!(p.target_ms(), grown - GROW_STEP_MS);
        // ONE near-miss — nobody heard anything yet — and the depth is back.
        p.step(want + pm, want);
        p.note_read(false);
        assert_eq!(
            p.target_ms(),
            grown,
            "a failed probe must restore the target on the first near-miss"
        );
    }

    /// After a failed probe the sync loop may not drive another shrink at the accelerated
    /// cadence — the slow, pre-sync window still applies, the five-second one does not.
    #[test]
    fn a_failed_probe_backs_the_sync_shrink_off() {
        let t = JitterTuning::COREAUDIO;
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(t, 2);
        for _ in 0..(2 * GROW_UNDERRUNS) {
            while !p.is_primed() {
                p.step(200 * pm, want);
            }
            p.step(200 * pm, want);
            p.note_read(true);
        }
        let grown = p.target_ms();
        p.set_sync_target(Some(pm));
        let depth = grown as usize * pm + want;
        // First sync-driven shrink, then fail its probe.
        while p.target_ms() == grown {
            p.step(depth, want);
            p.note_read(false);
        }
        p.step(want + pm, want);
        p.note_read(false);
        assert_eq!(p.target_ms(), grown, "restored");
        // Twice the accelerated window of clean audio: the backed-off loop must NOT have
        // shrunk again (before the fix this was exactly one audible failure per five seconds).
        for _ in 0..(2 * SHRINK_QUIET_SYNC_MS / 5) {
            p.step(depth, want);
            p.note_read(false);
        }
        assert_eq!(
            p.target_ms(),
            grown,
            "the accelerated cadence must be suspended after a failure"
        );
        // The slow pre-sync window still relaxes it eventually — backoff is not a freeze.
        for _ in 0..(2 * SHRINK_QUIET_MS / 5) {
            p.step(depth, want);
            p.note_read(false);
        }
        assert!(
            p.target_ms() < grown,
            "the slow window must still be allowed to test a shrink"
        );
    }

    /// One simulated bunching run's outcome.
    #[derive(Debug, Default)]
    struct BunchSim {
        /// Reads that actually starved the device — each one is audible.
        audible: u32,
        /// Audible reads in the second half of the run: non-zero means the policy never
        /// converged and the user hears it forever.
        audible_tail: u32,
    }

    /// Drive a policy over a link that BUNCHES: delivery pauses for `gap_ms` every `period_ms`,
    /// then the withheld audio arrives at once — the Wi-Fi power-save pattern from the field
    /// reports, where the total rate is fine and only the spacing is wrong. `drift_ppm` is the
    /// host-vs-DAC clock skew; a slightly slow host (negative) erodes the depth over minutes,
    /// which is what keeps re-testing whatever target the policy has settled on — without it a
    /// simulated ring freezes wherever priming left it and a wrong target is never punished.
    fn simulate_bunching(
        tuning: JitterTuning,
        sync_target: Option<usize>,
        ms: u32,
        gap_ms: u32,
        period_ms: u32,
        drift_ppm: i64,
    ) -> BunchSim {
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(tuning, 2);
        p.set_sync_target(sync_target);
        let mut depth = 0usize;
        let mut withheld = 0usize;
        let mut carry: i64 = 0;
        let mut out = BunchSim::default();
        for cb in 0..(ms / 5) {
            // The host keeps producing (want ± drift per callback); the link decides delivery.
            carry += want as i64 * drift_ppm;
            let extra = carry / 1_000_000;
            carry -= extra * 1_000_000;
            let produced = (want as i64 + extra).max(0) as usize;
            let in_gap = (cb * 5) % period_ms < gap_ms;
            if in_gap {
                withheld += produced;
            } else {
                depth += produced + std::mem::take(&mut withheld);
            }
            let s = p.step(depth, want);
            depth -= s.drop_front.min(depth);
            if s.silence {
                p.note_read(false);
                continue;
            }
            let short = depth < want;
            depth -= want.min(depth);
            if short {
                out.audible += 1;
                if cb >= ms / 10 {
                    out.audible_tail += 1;
                }
            }
            p.note_read(short);
        }
        out
    }

    /// THE field regression this whole change is for. A link that bunches needs ~30 ms of ring;
    /// the sync loop wants less. Before this change the policy paid an audible event nearly
    /// every bunching period, indefinitely — this exact simulation measured ~2000 over ten
    /// minutes: the sync loop re-probed a proven depth every five quiet seconds, growth needed
    /// three audible underruns to answer, and a grown target was never re-banked (growth raises
    /// a threshold; only a re-prime deepens the ring), so the depth rode the knife edge. Now
    /// near-misses grow the target before the first click, a failed shrink probe is undone at
    /// once and backs the sync loop off, and a hollow ring cashes the whole refill on the click
    /// it already paid. What remains is the clock-skew re-anchor — a slightly slow host
    /// genuinely starves the ring every few minutes, and only rate adaptation (which no client
    /// has) could remove that — so the bound is "a handful over ten minutes", not zero.
    #[test]
    fn sync_pressure_on_a_bunching_link_converges_instead_of_clicking_forever() {
        // 25 ms gaps every 300 ms, a slightly slow host, ten minutes, sync permanently asking
        // for a 5 ms ring.
        let s = simulate_bunching(
            JitterTuning::COREAUDIO,
            Some(per_ms(2) * 5),
            600_000,
            25,
            300,
            -50,
        );
        assert!(
            s.audible_tail <= 4,
            "the tug-of-war must converge to the skew floor: {s:?}"
        );
        assert!(
            s.audible <= 12,
            "learning the link may cost a handful of audible events, not a stream of them: {s:?}"
        );
    }

    /// The same link without sync pressure — the plain adaptive-growth behaviour — must land in
    /// the same place: sync steering may not add a persistent audible cost over not steering.
    #[test]
    fn a_bunching_link_without_sync_stays_clean_after_growing() {
        let s = simulate_bunching(JitterTuning::COREAUDIO, None, 600_000, 25, 300, -50);
        assert!(s.audible_tail <= 4, "{s:?}");
        assert!(s.audible <= 12, "{s:?}");
    }

    /// Unity must be bit-exact. The callers gate on `gain != 1.0` anyway, but if this ever stopped
    /// holding, every default session's wire would shift and the "byte-for-byte identical" claim
    /// the tier machinery rests on would quietly become false.
    #[test]
    fn unity_gain_is_bit_exact() {
        let src: Vec<f32> = (0..512).map(|i| (i as f32 / 512.0) * 2.0 - 1.0).collect();
        let mut got = src.clone();
        apply_gain(&mut got, 1.0);
        assert_eq!(got, src, "unity gain must not touch a single sample");
    }

    /// Below the knee the limiter is not in circuit at all: a boost whose peaks stay under
    /// `SOFT_LIMIT_KNEE` must be plain multiplication, or quiet material pays for a limiter it
    /// never needed.
    #[test]
    fn below_the_knee_is_plain_multiplication() {
        let mut got = vec![0.0, 0.1, -0.2, 0.34, -0.05];
        apply_gain(&mut got, 2.0);
        for (i, (g, s)) in got.iter().zip([0.0f32, 0.1, -0.2, 0.34, -0.05]).enumerate() {
            assert_eq!(*g, s * 2.0, "sample {i} must be untouched below the knee");
        }
    }

    /// The property the hard `clamp` violated and this exists to restore: no input, however
    /// absurdly gained, may leave the shaper out of range — and non-finite input must not escape
    /// as something the encoder would choke on.
    #[test]
    fn nothing_escapes_full_scale() {
        for gain in [1.5f32, 4.0, 8.0, 64.0, 1000.0] {
            let mut got: Vec<f32> = (0..401).map(|i| (i as f32 - 200.0) / 200.0).collect();
            apply_gain(&mut got, gain);
            for s in &got {
                assert!(s.abs() <= 1.0, "gain {gain} produced {s}");
            }
        }
        assert_eq!(soft_limit(f32::INFINITY), 1.0);
        assert_eq!(soft_limit(f32::NEG_INFINITY), -1.0);
    }

    /// Monotonic and odd-symmetric. Monotonicity is what keeps the shaper a limiter rather than a
    /// fold-back distortion; odd symmetry is what keeps its harmonics benign and its DC at zero.
    #[test]
    fn the_curve_is_monotonic_and_odd() {
        let mut prev = f32::NEG_INFINITY;
        for i in 0..=4000 {
            let x = (i as f32 - 2000.0) / 500.0; // -4.0 ..= 4.0
            let y = soft_limit(x);
            assert!(y >= prev, "not monotonic at {x}: {y} < {prev}");
            prev = y;
            assert!(
                (soft_limit(-x) + y).abs() < 1e-6,
                "not odd-symmetric at {x}"
            );
        }
    }

    /// The knee must not itself be an audible event. Both branches meet at the same value AND the
    /// same slope, so the transfer curve has no corner — a piecewise limiter that gets this wrong
    /// just swaps the clip's discontinuity for a softer one.
    #[test]
    fn the_knee_has_no_corner() {
        let k = SOFT_LIMIT_KNEE;
        assert!((soft_limit(k) - k).abs() < 1e-6, "value jumps at the knee");
        let h = 1e-4;
        let below = (soft_limit(k) - soft_limit(k - h)) / h;
        let above = (soft_limit(k + h) - soft_limit(k)) / h;
        assert!((below - 1.0).abs() < 1e-2, "linear side slope {below}");
        assert!(
            (above - below).abs() < 1e-2,
            "slope jumps at the knee: {below} -> {above}"
        );
    }
}
