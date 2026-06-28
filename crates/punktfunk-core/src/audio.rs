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
    /// Target Opus bitrate in bits/sec (hard CBR; constant packet size, which GameStream's
    /// audio FEC relies on).
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
}
