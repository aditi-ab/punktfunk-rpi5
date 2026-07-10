use super::*;
use crate::config::{CompositorPref, FecConfig, FecScheme, GamepadPref, Mode, Role};

#[test]
fn welcome_roundtrip() {
    let w = Welcome {
        abi_version: 1,
        udp_port: 9999,
        mode: Mode {
            width: 2560,
            height: 1440,
            refresh_hz: 240,
        },
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            fec_percent: 20,
            max_data_per_block: 4096,
        },
        shard_payload: 1200,
        encrypt: true,
        key: [7u8; 16],
        salt: [1, 2, 3, 4],
        frames: 600,
        compositor: CompositorPref::Gamescope,
        gamepad: GamepadPref::DualSense,
        bitrate_kbps: 50_000,
        bit_depth: 10,
        color: ColorInfo::HDR10_BT2020_PQ,
        chroma_format: CHROMA_IDC_444,
        audio_channels: 2,
        codec: CODEC_H264, // exercise a non-default codec through the roundtrip
        host_caps: HOST_CAP_GAMEPAD_STATE,
    };
    assert_eq!(Welcome::decode(&w.encode()).unwrap(), w);

    // Client-side reassembler ceiling derives from the negotiated rate: 4x the average frame at
    // 50 Mbps/240 Hz is ~104 KB, so the 8 MiB floor governs. The host keeps the p1_defaults
    // bound (it never reassembles video), as does a client of a bitrate-0 (older) host.
    let cc = w.session_config(Role::Client);
    assert_eq!(cc.max_frame_bytes, 8 << 20);
    cc.validate().expect("derived client config validates");
    assert_eq!(w.session_config(Role::Host).max_frame_bytes, 64 << 20);
    let old_host = Welcome {
        bitrate_kbps: 0,
        ..w
    };
    assert_eq!(
        old_host.session_config(Role::Client).max_frame_bytes,
        64 << 20
    );
    // A high-rate mode scales past the floor: 1.5 Gbps at 60 Hz = 4 x 3.125 MB = 12.5 MB.
    let fat = Welcome {
        bitrate_kbps: 1_500_000,
        mode: Mode {
            width: 5120,
            height: 1440,
            refresh_hz: 60,
        },
        ..w
    };
    let derived = fat.session_config(Role::Client).max_frame_bytes;
    assert_eq!(derived, 4 * 1_500_000 * 125 / 60);
    assert!(derived > (8 << 20) && derived < (64 << 20));
}

#[test]
fn codec_negotiation_and_back_compat() {
    // resolve_codec precedence (HEVC > AV1 > H.264), no preference (0).
    assert_eq!(
        resolve_codec(CODEC_H264 | CODEC_HEVC, CODEC_HEVC | CODEC_AV1, 0),
        Some(CODEC_HEVC)
    );
    assert_eq!(
        resolve_codec(CODEC_H264 | CODEC_AV1, CODEC_AV1 | CODEC_H264, 0),
        Some(CODEC_AV1)
    );
    assert_eq!(resolve_codec(CODEC_H264, CODEC_H264, 0), Some(CODEC_H264));
    // A software host (H.264 only) + an HEVC-only client share nothing → refuse.
    assert_eq!(resolve_codec(CODEC_HEVC, CODEC_H264, 0), None);
    // An older client (0 = no codec byte) is treated as HEVC-only.
    assert_eq!(
        resolve_codec(0, CODEC_HEVC | CODEC_H264, 0),
        Some(CODEC_HEVC)
    );
    assert_eq!(resolve_codec(0, CODEC_H264, 0), None);

    // Soft preference: honored when the host can also emit it, overriding precedence...
    assert_eq!(
        resolve_codec(CODEC_H264 | CODEC_HEVC, CODEC_H264 | CODEC_HEVC, CODEC_H264),
        Some(CODEC_H264)
    );
    assert_eq!(
        resolve_codec(CODEC_HEVC | CODEC_AV1, CODEC_HEVC | CODEC_AV1, CODEC_AV1),
        Some(CODEC_AV1)
    );
    // ...but falls back to precedence when the preferred codec isn't in the shared set.
    assert_eq!(
        resolve_codec(CODEC_HEVC | CODEC_H264, CODEC_HEVC | CODEC_H264, CODEC_AV1),
        Some(CODEC_HEVC)
    );
    // A preference the host can't emit still can't rescue a no-shared-codec case.
    assert_eq!(resolve_codec(CODEC_HEVC, CODEC_H264, CODEC_HEVC), None);

    // A Hello advertising codecs roundtrips, and the wire form of a codec-only Hello decodes on
    // a build that ignores the trailing byte (back-compat: extra bytes are skipped).
    let h = Hello {
        abi_version: 2,
        mode: Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        },
        compositor: CompositorPref::Auto,
        gamepad: GamepadPref::Auto,
        bitrate_kbps: 0,
        name: None,
        launch: None,
        video_caps: 0,
        audio_channels: 2, // stereo — forces the video_caps/audio_channels placeholders
        video_codecs: CODEC_H264 | CODEC_HEVC,
        preferred_codec: CODEC_H264,
    };
    let enc = h.encode();
    let dec = Hello::decode(&enc).unwrap();
    assert_eq!(dec.video_codecs, CODEC_H264 | CODEC_HEVC);
    assert_eq!(dec.preferred_codec, CODEC_H264);
    // Drop the preferred_codec byte → still decodes, video_codecs intact, preference gone.
    let no_pref = &enc[..enc.len() - 1];
    assert_eq!(
        Hello::decode(no_pref).unwrap().video_codecs,
        CODEC_H264 | CODEC_HEVC
    );
    assert_eq!(Hello::decode(no_pref).unwrap().preferred_codec, 0);
    // A pre-codec Hello (no video_codecs/preferred bytes) decodes to 0 → HEVC-only.
    let legacy = &enc[..enc.len() - 2];
    assert_eq!(Hello::decode(legacy).unwrap().video_codecs, 0);
    assert_eq!(Hello::decode(legacy).unwrap().preferred_codec, 0);

    // A pre-codec Welcome (no codec byte) decodes to HEVC.
    let mut w = Welcome::decode(
        &Welcome {
            abi_version: 2,
            udp_port: 1,
            mode: h.mode,
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 0,
                max_data_per_block: 1024,
            },
            shard_payload: 1024,
            encrypt: false,
            key: [0; 16],
            salt: [0; 4],
            frames: 0,
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            bit_depth: 8,
            color: ColorInfo::SDR_BT709,
            chroma_format: CHROMA_IDC_420,
            audio_channels: 2,
            codec: CODEC_H264,
            host_caps: 0,
        }
        .encode(),
    )
    .unwrap();
    assert_eq!(w.codec, CODEC_H264);
    w.codec = CODEC_HEVC;
    let wenc = w.encode();
    assert_eq!(
        Welcome::decode(&wenc[..wenc.len() - 1]).unwrap().codec,
        CODEC_HEVC
    );
}

#[test]
fn hdr_meta_datagram_roundtrip_and_truncation() {
    let m = HdrMeta {
        // BT.2020 display primaries in 1/50000 units (the DXGI/ST.2086 reference values).
        display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
        white_point: [15635, 16450],                 // D65
        max_display_mastering_luminance: 10_000_000, // 1000 nits in 0.0001 cd/m²
        min_display_mastering_luminance: 1,          // 0.0001 nits
        max_cll: 1000,
        max_fall: 400,
    };
    let d = encode_hdr_meta_datagram(&m);
    assert_eq!(d[0], HDR_META_MAGIC);
    assert_eq!(decode_hdr_meta_datagram(&d), Some(m));
    // Truncated buffers and a wrong tag are rejected (never partially read).
    for n in 0..d.len() {
        assert_eq!(decode_hdr_meta_datagram(&d[..n]), None);
    }
    let mut bad = d.clone();
    bad[0] = HIDOUT_MAGIC;
    assert_eq!(decode_hdr_meta_datagram(&bad), None);
}

#[test]
fn host_timing_datagram_roundtrip_and_truncation() {
    let t = HostTiming {
        pts_ns: 1_751_500_000_123_456_789, // a realistic 2026 CLOCK_REALTIME capture stamp
        host_us: 4_321,
    };
    let d = encode_host_timing_datagram(&t);
    assert_eq!(d[0], HOST_TIMING_MAGIC);
    assert_eq!(d.len(), 13);
    assert_eq!(decode_host_timing_datagram(&d), Some(t));
    // Truncated buffers and a wrong tag are rejected (never partially read).
    for n in 0..d.len() {
        assert_eq!(decode_host_timing_datagram(&d[..n]), None);
    }
    let mut bad = d.clone();
    bad[0] = HDR_META_MAGIC;
    assert_eq!(decode_host_timing_datagram(&bad), None);
}

#[test]
fn hello_start_roundtrip() {
    let h = Hello {
        abi_version: 1,
        mode: Mode {
            width: 1280,
            height: 720,
            refresh_hz: 120,
        },
        compositor: CompositorPref::Kwin,
        gamepad: GamepadPref::DualSense,
        bitrate_kbps: 25_000,
        name: Some("Test Device".into()),
        launch: Some("steam:570".into()),
        video_caps: VIDEO_CAP_10BIT,
        audio_channels: 2,
        video_codecs: CODEC_H264 | CODEC_HEVC, // exercise the codec bitfield roundtrip
        preferred_codec: CODEC_HEVC,
    };
    assert_eq!(Hello::decode(&h.encode()).unwrap(), h);
    let s = Start {
        client_udp_port: 1234,
    };
    assert_eq!(Start::decode(&s.encode()).unwrap(), s);
}

#[test]
fn compositor_pref_wire_and_names() {
    for p in [
        CompositorPref::Auto,
        CompositorPref::Kwin,
        CompositorPref::Wlroots,
        CompositorPref::Mutter,
        CompositorPref::Gamescope,
    ] {
        assert_eq!(CompositorPref::from_u8(p.to_u8()), p);
        assert_eq!(CompositorPref::from_name(p.as_str()), Some(p));
    }
    // Aliases + unknowns.
    assert_eq!(CompositorPref::from_name("KDE"), Some(CompositorPref::Kwin));
    assert_eq!(
        CompositorPref::from_name("sway"),
        Some(CompositorPref::Wlroots)
    );
    assert_eq!(CompositorPref::from_name("nope"), None);
    // Unknown wire byte degrades to Auto (forward-compatible).
    assert_eq!(CompositorPref::from_u8(200), CompositorPref::Auto);
}

#[test]
fn gamepad_pref_wire_and_names() {
    for p in [
        GamepadPref::Auto,
        GamepadPref::Xbox360,
        GamepadPref::DualSense,
        GamepadPref::XboxOne,
        GamepadPref::DualShock4,
    ] {
        assert_eq!(GamepadPref::from_u8(p.to_u8()), p);
        assert_eq!(GamepadPref::from_name(p.as_str()), Some(p));
    }
    // Distinct wire bytes (forward-compat with peers that only know 0..=2).
    assert_eq!(GamepadPref::XboxOne.to_u8(), 3);
    assert_eq!(GamepadPref::DualShock4.to_u8(), 4);
    // Aliases + unknowns.
    assert_eq!(GamepadPref::from_name("PS5"), Some(GamepadPref::DualSense));
    assert_eq!(GamepadPref::from_name("x360"), Some(GamepadPref::Xbox360));
    assert_eq!(GamepadPref::from_name("ps4"), Some(GamepadPref::DualShock4));
    assert_eq!(GamepadPref::from_name("DS4"), Some(GamepadPref::DualShock4));
    assert_eq!(
        GamepadPref::from_name("xbox-one"),
        Some(GamepadPref::XboxOne)
    );
    assert_eq!(GamepadPref::from_name("series"), Some(GamepadPref::XboxOne));
    assert_eq!(GamepadPref::from_name("nope"), None);
    // Unknown wire byte degrades to Auto (forward-compatible).
    assert_eq!(GamepadPref::from_u8(200), GamepadPref::Auto);
}

#[test]
fn hello_welcome_compositor_back_compat() {
    // Trailing optional bytes (compositor at 20/53, gamepad at 21/54): a legacy peer's
    // shorter message still decodes (missing fields = Auto), and a legacy peer reading a
    // new message ignores the trailing bytes. Simulate both directions by truncation.
    let h = Hello {
        abi_version: 2,
        mode: Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        },
        compositor: CompositorPref::Mutter,
        gamepad: GamepadPref::DualSense,
        bitrate_kbps: 80_000,
        name: None,
        launch: None,
        video_caps: 0,
        audio_channels: 2,
        video_codecs: 0,
        preferred_codec: 0,
    };
    let enc = h.encode();
    assert_eq!(enc.len(), 26);
    // Legacy (20-byte) Hello → both Auto, no bitrate, mode intact.
    let legacy = Hello::decode(&enc[..20]).unwrap();
    assert_eq!(legacy.compositor, CompositorPref::Auto);
    assert_eq!(legacy.gamepad, GamepadPref::Auto);
    assert_eq!(legacy.bitrate_kbps, 0);
    assert_eq!(legacy.mode, h.mode);
    // Compositor-era (21-byte) Hello → compositor intact, gamepad Auto.
    let mid = Hello::decode(&enc[..21]).unwrap();
    assert_eq!(mid.compositor, CompositorPref::Mutter);
    assert_eq!(mid.gamepad, GamepadPref::Auto);
    // Gamepad-era (22-byte) Hello → compositor + gamepad intact, bitrate 0 (host default).
    let pre_bitrate = Hello::decode(&enc[..22]).unwrap();
    assert_eq!(pre_bitrate.gamepad, GamepadPref::DualSense);
    assert_eq!(pre_bitrate.bitrate_kbps, 0);
    // Full message → bitrate intact.
    assert_eq!(Hello::decode(&enc).unwrap().bitrate_kbps, 80_000);

    let w = Welcome {
        abi_version: 2,
        udp_port: 7000,
        mode: h.mode,
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            fec_percent: 20,
            max_data_per_block: 4096,
        },
        shard_payload: 1200,
        encrypt: true,
        key: [3u8; 16],
        salt: [9, 8, 7, 6],
        frames: 0,
        compositor: CompositorPref::Kwin,
        gamepad: GamepadPref::Xbox360,
        bitrate_kbps: 120_000,
        bit_depth: 10,
        color: ColorInfo::HDR10_BT2020_PQ,
        chroma_format: CHROMA_IDC_444,
        audio_channels: 6, // 5.1 — exercises the non-default trailing byte
        codec: CODEC_HEVC,
        host_caps: HOST_CAP_GAMEPAD_STATE,
    };
    let wenc = w.encode();
    assert_eq!(wenc.len(), 68); // 60 base + 4 colour + chroma + audio-channels + codec + host-caps
    let legacy_w = Welcome::decode(&wenc[..53]).unwrap();
    assert_eq!(legacy_w.compositor, CompositorPref::Auto);
    assert_eq!(legacy_w.gamepad, GamepadPref::Auto);
    assert_eq!(legacy_w.bitrate_kbps, 0);
    assert_eq!(legacy_w.frames, 0);
    assert_eq!(legacy_w.key, w.key);
    let mid_w = Welcome::decode(&wenc[..54]).unwrap();
    assert_eq!(mid_w.compositor, CompositorPref::Kwin);
    assert_eq!(mid_w.gamepad, GamepadPref::Auto);
    // Gamepad-era (55-byte) Welcome → gamepad intact, bitrate 0 (unknown).
    let pre_bitrate_w = Welcome::decode(&wenc[..55]).unwrap();
    assert_eq!(pre_bitrate_w.gamepad, GamepadPref::Xbox360);
    assert_eq!(pre_bitrate_w.bitrate_kbps, 0);
    assert_eq!(pre_bitrate_w.bit_depth, 8); // older host (no trailing byte) → 8-bit assumed
    assert_eq!(legacy_w.bit_depth, 8);
    // A pre-colour (60-byte) Welcome → SDR BT.709 (the only colour those hosts produced).
    let pre_color_w = Welcome::decode(&wenc[..60]).unwrap();
    assert_eq!(pre_color_w.bit_depth, 10);
    assert_eq!(pre_color_w.color, ColorInfo::SDR_BT709);
    assert_eq!(pre_color_w.chroma_format, CHROMA_IDC_420); // pre-chroma host → 4:2:0
    assert_eq!(legacy_w.color, ColorInfo::SDR_BT709);
    assert_eq!(legacy_w.chroma_format, CHROMA_IDC_420);
    // A pre-chroma (64-byte) Welcome carries colour but no chroma/audio bytes → 4:2:0 + stereo.
    let pre_chroma_w = Welcome::decode(&wenc[..64]).unwrap();
    assert_eq!(pre_chroma_w.color, ColorInfo::HDR10_BT2020_PQ);
    assert_eq!(pre_chroma_w.chroma_format, CHROMA_IDC_420);
    assert_eq!(pre_chroma_w.audio_channels, 2); // audio byte (offset 65) absent → stereo
                                                // A pre-audio (65-byte) Welcome carries chroma but no audio byte → 4:4:4 + stereo.
    let pre_audio_w = Welcome::decode(&wenc[..65]).unwrap();
    assert_eq!(pre_audio_w.chroma_format, CHROMA_IDC_444);
    assert_eq!(pre_audio_w.audio_channels, 2);
    assert_eq!(Welcome::decode(&wenc).unwrap().bitrate_kbps, 120_000);
    assert_eq!(Welcome::decode(&wenc).unwrap().bit_depth, 10); // full form carries it
    assert_eq!(
        Welcome::decode(&wenc).unwrap().color,
        ColorInfo::HDR10_BT2020_PQ
    );
    assert_eq!(
        Welcome::decode(&wenc).unwrap().chroma_format,
        CHROMA_IDC_444
    ); // full form carries 4:4:4
    assert_eq!(Welcome::decode(&wenc).unwrap().audio_channels, 6); // ...and 5.1
                                                                   // A pre-host-caps (67-byte) Welcome → 0 (legacy input only); the full form carries the bit.
    assert_eq!(Welcome::decode(&wenc[..67]).unwrap().host_caps, 0);
    assert_eq!(
        Welcome::decode(&wenc).unwrap().host_caps,
        HOST_CAP_GAMEPAD_STATE
    );
}

#[test]
fn hello_name_roundtrip_and_back_compat() {
    let base = Hello {
        abi_version: 2,
        mode: Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        },
        compositor: CompositorPref::Auto,
        gamepad: GamepadPref::Auto,
        bitrate_kbps: 0,
        name: Some("Enrico's MacBook".into()),
        launch: None,
        video_caps: 0,
        audio_channels: 2,
        video_codecs: 0,
        preferred_codec: 0,
    };
    let enc = base.encode();
    assert_eq!(
        Hello::decode(&enc).unwrap().name.as_deref(),
        Some("Enrico's MacBook")
    );
    // A bitrate-era (26-byte) peer reading a named Hello ignores the trailing name; a named
    // host reading a bitrate-era Hello decodes name = None.
    assert_eq!(Hello::decode(&enc[..26]).unwrap().name, None);
    // No name → wire form is byte-identical to the bitrate-era message (26 bytes).
    let unnamed = Hello {
        name: None,
        ..base.clone()
    };
    assert_eq!(unnamed.encode().len(), 26);
    // Over-long names truncate to a char boundary within HELLO_NAME_MAX on encode.
    let long = Hello {
        name: Some(format!("{}ü", "x".repeat(HELLO_NAME_MAX - 1))), // ü straddles the cap
        ..base.clone()
    };
    let dec = Hello::decode(&long.encode()).unwrap();
    let n = dec.name.expect("truncated name still present");
    assert!(n.len() <= HELLO_NAME_MAX && n.starts_with('x'));
    // A corrupt length byte (longer than the buffer) or bad UTF-8 degrades to None, never Err.
    let mut bad_len = unnamed.encode();
    bad_len.push(40); // claims 40 name bytes, none follow
    assert_eq!(Hello::decode(&bad_len).unwrap().name, None);
    let mut bad_utf8 = unnamed.encode();
    bad_utf8.extend_from_slice(&[2, 0xFF, 0xFE]);
    assert_eq!(Hello::decode(&bad_utf8).unwrap().name, None);
}

#[test]
fn hello_launch_roundtrip_and_back_compat() {
    let base = Hello {
        abi_version: 2,
        mode: Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        },
        compositor: CompositorPref::Auto,
        gamepad: GamepadPref::Auto,
        bitrate_kbps: 0,
        name: None,
        launch: None,
        video_caps: 0,
        audio_channels: 2,
        video_codecs: 0,
        preferred_codec: 0,
    };
    // launch alone (no name): a zero-length name placeholder keeps the offset deterministic.
    let with_launch = Hello {
        launch: Some("steam:570".into()),
        ..base.clone()
    };
    assert_eq!(Hello::decode(&with_launch.encode()).unwrap(), with_launch);
    // launch + name together.
    let both = Hello {
        name: Some("Enrico's Mac".into()),
        launch: Some("custom:abc123".into()),
        ..base.clone()
    };
    assert_eq!(Hello::decode(&both.encode()).unwrap(), both);
    // name but no launch (a name-era client): launch decodes None.
    let name_only = Hello {
        name: Some("Enrico's Mac".into()),
        ..base.clone()
    };
    assert_eq!(Hello::decode(&name_only.encode()).unwrap().launch, None);
    // Neither field → still the 26-byte bitrate-era form (no launch placeholder emitted).
    assert_eq!(base.encode().len(), 26);
    assert_eq!(Hello::decode(&base.encode()).unwrap().launch, None);
    // A bitrate-era (26-byte) peer reading a launch-bearing Hello ignores it.
    assert_eq!(
        Hello::decode(&with_launch.encode()[..26]).unwrap().launch,
        None
    );
    // Over-long ids truncate on a char boundary within HELLO_LAUNCH_MAX.
    let long = Hello {
        launch: Some(format!("{}ü", "x".repeat(HELLO_LAUNCH_MAX - 1))),
        ..base.clone()
    };
    let dec = Hello::decode(&long.encode())
        .unwrap()
        .launch
        .expect("present");
    assert!(dec.len() <= HELLO_LAUNCH_MAX && dec.starts_with('x'));
}

#[test]
fn reconfigure_roundtrip() {
    let rq = Reconfigure {
        mode: Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 144,
        },
    };
    assert_eq!(Reconfigure::decode(&rq.encode()).unwrap(), rq);
    for accepted in [true, false] {
        let rs = Reconfigured {
            accepted,
            mode: rq.mode,
        };
        assert_eq!(Reconfigured::decode(&rs.encode()).unwrap(), rs);
    }
    // The type byte separates the post-handshake messages from each other.
    assert!(Reconfigure::decode(
        &Reconfigured {
            accepted: true,
            mode: rq.mode
        }
        .encode()
    )
    .is_err());
}

#[test]
fn request_keyframe_roundtrip() {
    let bytes = RequestKeyframe.encode();
    assert!(RequestKeyframe::decode(&bytes).is_ok());
    // Distinct from the other control messages — its type byte must not collide.
    let mode = Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };
    assert!(RequestKeyframe::decode(&Reconfigure { mode }.encode()).is_err());
    assert!(Reconfigure::decode(&bytes).is_err());
    // Length is exact (no trailing bytes accepted).
    assert!(RequestKeyframe::decode(&[bytes.as_slice(), &[0]].concat()).is_err());
}

#[test]
fn loss_report_roundtrip() {
    for loss_ppm in [0u32, 1, 12_345, 50_000, 1_000_000] {
        let r = LossReport { loss_ppm };
        assert_eq!(LossReport::decode(&r.encode()).unwrap(), r);
    }
    // Disjoint from the other control messages (type byte + length).
    assert!(LossReport::decode(&RequestKeyframe.encode()).is_err());
    assert!(RequestKeyframe::decode(&LossReport { loss_ppm: 0 }.encode()).is_err());
    assert!(
        LossReport::decode(&[LossReport { loss_ppm: 0 }.encode().as_slice(), &[0]].concat())
            .is_err()
    );
}

#[test]
fn window_loss_ppm_estimates_and_caps() {
    // No traffic → 0. A clean window (nothing recovered) → 0.
    assert_eq!(window_loss_ppm(0, 0, 0), 0);
    assert_eq!(window_loss_ppm(0, 1000, 0), 0);
    // 50 recovered of 1000 total (950 received + 50 recovered) = 5%.
    assert_eq!(window_loss_ppm(50, 950, 0), 50_000);
    // An unrecoverable frame adds the +5% bump (push FEC past the current cap).
    assert_eq!(window_loss_ppm(50, 950, 1), 100_000);
    // A total-loss window with a drop but nothing received still reports the bump, capped at 1e6.
    assert_eq!(window_loss_ppm(0, 0, 3), 50_000);
    assert!(window_loss_ppm(u64::MAX, 1, 9) <= 1_000_000);
}

#[test]
fn bitrate_messages_roundtrip() {
    let req = SetBitrate {
        bitrate_kbps: 14_000,
    };
    assert_eq!(SetBitrate::decode(&req.encode()).unwrap(), req);
    let ack = BitrateChanged {
        bitrate_kbps: 14_000,
    };
    assert_eq!(BitrateChanged::decode(&ack.encode()).unwrap(), ack);
    // Same payload shape as LossReport — the type byte alone must keep them disjoint.
    assert!(LossReport::decode(&req.encode()).is_err());
    assert!(SetBitrate::decode(&ack.encode()).is_err());
    assert!(BitrateChanged::decode(&req.encode()).is_err());
    assert!(SetBitrate::decode(&LossReport { loss_ppm: 7 }.encode()).is_err());
}

#[test]
fn probe_messages_roundtrip() {
    let req = ProbeRequest {
        target_kbps: 250_000,
        duration_ms: 2000,
    };
    assert_eq!(ProbeRequest::decode(&req.encode()).unwrap(), req);
    let res = ProbeResult {
        bytes_sent: 62_500_000,
        packets_sent: 480,
        duration_ms: 2003,
        wire_packets_sent: 41_000,
        send_dropped: 1_200,
    };
    assert_eq!(ProbeResult::decode(&res.encode()).unwrap(), res);
    assert_eq!(res.encode().len(), 29);
    // A pre-wire-stats host's 21-byte ProbeResult still decodes, with the new fields zeroed.
    let legacy = {
        let full = res.encode();
        full[..21].to_vec()
    };
    let decoded = ProbeResult::decode(&legacy).unwrap();
    assert_eq!(decoded.wire_packets_sent, 0);
    assert_eq!(decoded.send_dropped, 0);
    assert_eq!(decoded.bytes_sent, res.bytes_sent);
    // Type bytes keep the control messages disjoint from each other.
    assert!(ProbeRequest::decode(&res.encode()).is_err());
    assert!(Reconfigure::decode(&req.encode()).is_err());
    assert!(ProbeResult::decode(&req.encode()).is_err());
}

#[test]
fn clock_messages_roundtrip() {
    let probe = ClockProbe {
        t1_ns: 1_700_000_000_123,
    };
    assert_eq!(ClockProbe::decode(&probe.encode()).unwrap(), probe);
    let echo = ClockEcho {
        t1_ns: 1_700_000_000_123,
        t2_ns: 1_700_000_050_456,
        t3_ns: 1_700_000_050_789,
    };
    assert_eq!(ClockEcho::decode(&echo.encode()).unwrap(), echo);
    // Disjoint from the other control messages (distinct type bytes).
    assert!(ClockProbe::decode(&echo.encode()).is_err());
    assert!(ProbeRequest::decode(&probe.encode()).is_err());
    assert!(ClockEcho::decode(&probe.encode()).is_err());
}

#[test]
fn clock_offset_picks_min_rtt_and_recovers_offset() {
    // Host clock is +1_000_000 ns ahead of the client. Construct samples where a symmetric
    // round-trip recovers exactly that offset, and a noisy (asymmetric, high-RTT) sample is
    // present but must be ignored by the min-RTT selection.
    const OFF: i64 = 1_000_000;
    // Clean sample: client t1=0, one-way=200µs each way → t2 = t1 + 200_000 + OFF (host clock),
    // t3 = t2 + 50_000 (host processing), t4 = t3 - OFF + 200_000 (back in client clock).
    let t1 = 0u64;
    let t2 = (t1 as i64 + 200_000 + OFF) as u64;
    let t3 = t2 + 50_000;
    let t4 = (t3 as i64 - OFF + 200_000) as u64;
    // Noisy sample: same offset but a fat, asymmetric RTT (slow return path) — higher RTT.
    let n1 = 1_000_000u64;
    let n2 = (n1 as i64 + 200_000 + OFF) as u64;
    let n3 = n2 + 50_000;
    let n4 = (n3 as i64 - OFF + 5_000_000) as u64; // 5 ms return → big RTT
    let (offset, rtt) = clock_offset_ns(&[(n1, n2, n3, n4), (t1, t2, t3, t4)]).expect("non-empty");
    // The min-RTT sample recovers the offset exactly; its RTT is 2x200us, and the noisy
    // (asymmetric, 5 ms return) sample is ignored by the min-RTT selection.
    assert_eq!(offset, OFF);
    assert_eq!(rtt, 400_000);
    assert!(clock_offset_ns(&[]).is_none());
}

/// The mid-stream re-sync state machine: 8 rounds collected via matched echoes, stale
/// echoes ignored, a restarted batch abandons the old one, and the batch result is the
/// min-RTT estimate — the exact behavior the connect-time `clock_sync` loop has.
#[test]
fn clock_resync_collects_rounds_and_ignores_stale_echoes() {
    // Host clock +1 ms ahead; symmetric 100 µs one-way paths except one congested round.
    const OFF: i64 = 1_000_000;
    let echo_for = |t1: u64, one_way: u64| ClockEcho {
        t1_ns: t1,
        t2_ns: (t1 as i64 + one_way as i64 + OFF) as u64,
        t3_ns: (t1 as i64 + one_way as i64 + OFF) as u64 + 10_000,
    };
    let t4_for = |e: &ClockEcho, one_way: u64| (e.t3_ns as i64 - OFF + one_way as i64) as u64;

    let mut rs = ClockResync::new();
    // An unsolicited echo before any batch is ignored.
    assert_eq!(
        rs.on_echo(&echo_for(42, 100_000), 500_000),
        ResyncStep::Idle
    );

    let mut probe = rs.begin(1_000_000);
    // A stale echo (wrong t1: the abandoned pre-begin probe) is ignored mid-batch.
    assert_eq!(
        rs.on_echo(&echo_for(42, 100_000), 500_000),
        ResyncStep::Idle
    );
    for round in 0..ClockResync::ROUNDS {
        // Round 3 is congested (5 ms one-way) — it must lose the min-RTT selection.
        let one_way = if round == 3 { 5_000_000 } else { 100_000 };
        let echo = echo_for(probe.t1_ns, one_way);
        let t4 = t4_for(&echo, one_way);
        match rs.on_echo(&echo, t4) {
            ResyncStep::Probe(p) => {
                assert!(round < ClockResync::ROUNDS - 1, "batch overran its rounds");
                probe = p;
            }
            ResyncStep::Done { offset_ns, rtt_ns } => {
                assert_eq!(round, ClockResync::ROUNDS - 1, "batch ended early");
                assert_eq!(offset_ns, OFF, "min-RTT round recovers the offset exactly");
                assert_eq!(rtt_ns, 200_000); // 2×100 µs; host processing (t3−t2) excluded
            }
            ResyncStep::Idle => panic!("matched echo must advance the batch"),
        }
    }
    // The batch is done: even a matching-t1 replay no longer advances anything.
    assert_eq!(
        rs.on_echo(&echo_for(probe.t1_ns, 100_000), probe.t1_ns + 300_000),
        ResyncStep::Idle
    );

    // begin() mid-batch abandons the in-flight batch: its echo is stale afterwards.
    let old = rs.begin(2_000_000);
    let fresh = rs.begin(3_000_000);
    assert_eq!(
        rs.on_echo(&echo_for(old.t1_ns, 100_000), 2_300_000),
        ResyncStep::Idle
    );
    assert!(matches!(
        rs.on_echo(&echo_for(fresh.t1_ns, 100_000), 3_300_000),
        ResyncStep::Probe(_)
    ));
}

/// The acceptance guard: a batch measured through a congested window (fat RTT) must not
/// replace the offset — its queueing delay biases the estimate exactly when frames
/// already read late. Floor of 2 ms so a near-zero connect RTT (same-host/LAN) doesn't
/// reject every later batch over normal jitter.
#[test]
fn clock_resync_acceptance_guard() {
    // Generous connect RTT (10 ms): accept up to 1.5×.
    assert!(accept_resync(14_000_000, 10_000_000));
    assert!(!accept_resync(16_000_000, 10_000_000));
    // Tiny connect RTT (200 µs, wired LAN): the 2 ms floor governs.
    assert!(accept_resync(1_900_000, 200_000));
    assert!(!accept_resync(2_100_000, 200_000));
    // Boundary: exactly at the bound is accepted.
    assert!(accept_resync(2_000_000, 0));
    assert!(accept_resync(15_000_000, 10_000_000));
}

#[test]
fn control_messages_disjoint_from_hello() {
    // A Hello uses MAGIC (PKF1); control messages use CTL_MAGIC (PKFc). No Hello — at
    // any abi_version — can be misparsed as a control message, and vice-versa.
    for abi in [1u32, 2, 16, 0x10, 0x0113, 0x1410] {
        let h = Hello {
            abi_version: abi,
            mode: Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
        }
        .encode();
        assert!(PairRequest::decode(&h).is_err(), "abi {abi} parsed as pair");
        assert!(Reconfigure::decode(&h).is_err());
    }
    // And a PairRequest never parses as a Hello.
    let pr = PairRequest {
        name: "x".into(),
        spake_a: vec![0u8; 33],
    }
    .encode();
    assert!(Hello::decode(&pr).is_err());
}

#[test]
fn pair_messages_roundtrip() {
    let pr = PairRequest {
        name: "Enrico's Mac".into(),
        spake_a: vec![1, 2, 3, 4, 5],
    };
    assert_eq!(PairRequest::decode(&pr.encode()).unwrap(), pr);
    let pc = PairChallenge {
        spake_b: vec![9; 33],
        confirm: [7u8; 32],
    };
    assert_eq!(PairChallenge::decode(&pc.encode()).unwrap(), pc);
    let pp = PairProof { confirm: [3u8; 32] };
    assert_eq!(PairProof::decode(&pp.encode()).unwrap(), pp);
    for ok in [true, false] {
        assert_eq!(
            PairResult::decode(&PairResult { ok }.encode()).unwrap().ok,
            ok
        );
    }
    // Length-exact: a truncated/padded PairProof is rejected.
    let mut bad = pp.encode();
    bad.push(0);
    assert!(PairProof::decode(&bad).is_err());
}

#[test]
fn spake2_pairing_agrees_only_on_matching_pin_and_certs() {
    let cfp = [0x11u8; 32];
    let hfp = [0x22u8; 32];

    // Right PIN, same fingerprint views on both sides → both confirmations agree.
    let (ca, ma) = pake::start(true, "4321", &cfp, &hfp);
    let (cb, mb) = pake::start(false, "4321", &cfp, &hfp);
    let a = ca.finish(&mb).unwrap();
    let b = cb.finish(&ma).unwrap();
    assert!(pake::verify(&a.host, &b.host) && pake::verify(&a.client, &b.client));

    // Wrong PIN → different keys → confirmations DON'T match (one online guess wasted).
    let (ca, ma) = pake::start(true, "0000", &cfp, &hfp);
    let (cb, mb) = pake::start(false, "4321", &cfp, &hfp);
    let a = ca.finish(&mb).unwrap();
    let b = cb.finish(&ma).unwrap();
    assert!(!pake::verify(&a.client, &b.client));

    // MITM: the two legs saw different host certs → no agreement even with the right PIN.
    let attacker_hfp = [0x33u8; 32];
    let (ca, ma) = pake::start(true, "4321", &cfp, &attacker_hfp);
    let (cb, mb) = pake::start(false, "4321", &cfp, &hfp);
    let a = ca.finish(&mb).unwrap();
    let b = cb.finish(&ma).unwrap();
    assert!(!pake::verify(&a.client, &b.client));
}

#[test]
fn audio_datagram_roundtrip() {
    let opus = [0x42u8; 97];
    let d = encode_audio_datagram(7, 1_000_000_123, &opus);
    assert_eq!(d[0], AUDIO_MAGIC);
    let (seq, pts, payload) = decode_audio_datagram(&d).unwrap();
    assert_eq!((seq, pts), (7, 1_000_000_123));
    assert_eq!(payload, opus);
    assert!(decode_audio_datagram(&d[..12]).is_none()); // truncated header
    assert!(decode_audio_datagram(&[0u8; 13]).is_none()); // bad magic

    // Empty payload is legal (DTX) — header-only datagram.
    let header_only = encode_audio_datagram(0, 0, &[]);
    let (_, _, empty) = decode_audio_datagram(&header_only).unwrap();
    assert!(empty.is_empty());
}

#[test]
fn rumble_datagram_roundtrip() {
    let d = encode_rumble_datagram(1, 0x1234, 0xFFFF);
    assert_eq!(d[0], RUMBLE_MAGIC);
    assert_eq!(decode_rumble_datagram(&d), Some((1, 0x1234, 0xFFFF)));
    assert!(decode_rumble_datagram(&d[..6]).is_none());
}

#[test]
fn mic_datagram_roundtrip_and_disjoint_from_audio() {
    let opus = [0x5Au8; 80];
    let d = encode_mic_datagram(42, 9_999, &opus);
    assert_eq!(d[0], MIC_MAGIC);
    let (seq, pts, payload) = decode_mic_datagram(&d).unwrap();
    assert_eq!((seq, pts), (42, 9_999));
    assert_eq!(payload, opus);
    assert!(decode_mic_datagram(&d[..12]).is_none()); // truncated
                                                      // Tag separation: a mic datagram is not an audio datagram and vice-versa.
    assert!(decode_audio_datagram(&d).is_none());
    assert!(decode_mic_datagram(&encode_audio_datagram(1, 2, &opus)).is_none());
    // Empty payload (DTX) is legal.
    assert!(decode_mic_datagram(&encode_mic_datagram(0, 0, &[]))
        .unwrap()
        .2
        .is_empty());
}

#[test]
fn rich_input_roundtrip() {
    for ev in [
        RichInput::Touchpad {
            pad: 1,
            finger: 0,
            active: true,
            x: 40000,
            y: 12345,
        },
        RichInput::Motion {
            pad: 0,
            gyro: [-100, 200, -300],
            accel: [16384, -8192, 1],
        },
        RichInput::TouchpadEx {
            pad: 2,
            surface: 1,
            finger: 1,
            touch: true,
            click: false,
            x: -12345,
            y: 30000,
            pressure: 4000,
        },
    ] {
        let d = ev.encode();
        assert_eq!(d[0], RICH_INPUT_MAGIC);
        assert_eq!(RichInput::decode(&d), Some(ev));
    }
    // Disjoint from the fixed input datagram (0xC8); unknown kind + truncation → None.
    assert!(RichInput::decode(&[crate::input::INPUT_MAGIC; 18]).is_none());
    assert!(RichInput::decode(&[RICH_INPUT_MAGIC, 0x7F]).is_none()); // unknown kind
    assert!(RichInput::decode(&[RICH_INPUT_MAGIC, RICH_TOUCHPAD, 0]).is_none()); // short
    assert!(RichInput::decode(&[RICH_INPUT_MAGIC, RICH_TOUCHPAD_EX, 0, 0, 0, 0]).is_none());
    // short
}

#[test]
fn hid_output_roundtrip() {
    let cases = [
        HidOutput::Led {
            pad: 2,
            r: 0xAA,
            g: 0xBB,
            b: 0xCC,
        },
        HidOutput::PlayerLeds {
            pad: 0,
            bits: 0b10101,
        },
        HidOutput::Trigger {
            pad: 1,
            which: 1,
            effect: vec![0x26, 0x90, 0xA0, 0xFF, 0x00, 0x00],
        },
        HidOutput::TrackpadHaptic {
            pad: 0,
            side: 1,
            amplitude: 0x1234,
            period: 0x5678,
            count: 9,
        },
    ];
    for ev in &cases {
        let d = ev.encode();
        assert_eq!(d[0], HIDOUT_MAGIC);
        assert_eq!(HidOutput::decode(&d).as_ref(), Some(ev));
    }
    assert!(HidOutput::decode(&[HIDOUT_MAGIC, 0x7F]).is_none()); // unknown kind
                                                                 // A rich-input datagram is not a HID-output datagram.
    assert!(HidOutput::decode(
        &RichInput::Motion {
            pad: 0,
            gyro: [0; 3],
            accel: [0; 3]
        }
        .encode()
    )
    .is_none());
}

#[test]
fn fingerprint_is_sha256_of_der() {
    // Stable across calls, distinct for distinct certs.
    let a = endpoint::cert_fingerprint(b"cert-a");
    assert_eq!(a, endpoint::cert_fingerprint(b"cert-a"));
    assert_ne!(a, endpoint::cert_fingerprint(b"cert-b"));
}
