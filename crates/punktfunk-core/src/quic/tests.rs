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

    // PyroWave is opt-in ONLY (plan §3): mutual support NEVER auto-selects it — the ladder
    // ignores it entirely...
    assert_eq!(
        resolve_codec(CODEC_HEVC | CODEC_PYROWAVE, CODEC_HEVC | CODEC_PYROWAVE, 0),
        Some(CODEC_HEVC)
    );
    // ...even when it is the ONLY shared codec (an all-intra 200 Mbps stream must never be a
    // silent fallback)...
    assert_eq!(resolve_codec(CODEC_PYROWAVE, CODEC_PYROWAVE, 0), None);
    // ...it is reachable exclusively through the client's explicit preference.
    assert_eq!(
        resolve_codec(
            CODEC_HEVC | CODEC_PYROWAVE,
            CODEC_HEVC | CODEC_PYROWAVE,
            CODEC_PYROWAVE
        ),
        Some(CODEC_PYROWAVE)
    );
    // A pyrowave preference against a host without the backend falls back to the ladder.
    assert_eq!(
        resolve_codec(CODEC_HEVC | CODEC_PYROWAVE, CODEC_HEVC, CODEC_PYROWAVE),
        Some(CODEC_HEVC)
    );
    // And the negotiated bit SURVIVES the Welcome wire roundtrip — the decode whitelist
    // once folded unknown codec bytes (incl. PyroWave) to HEVC, which sent wavelet AUs
    // into an FFmpeg HEVC decoder on the first on-glass run.
    let mut pw_w = Welcome::decode(
        &Welcome {
            abi_version: 2,
            udp_port: 1,
            mode: Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
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
            codec: CODEC_PYROWAVE,
            host_caps: 0,
        }
        .encode(),
    )
    .unwrap();
    assert_eq!(pw_w.codec, CODEC_PYROWAVE);
    // A genuinely unknown future bit still folds to the HEVC default.
    pw_w.codec = 0x40;
    assert_eq!(Welcome::decode(&pw_w.encode()).unwrap().codec, CODEC_HEVC);

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
        display_hdr: None,
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
        display_hdr: None,
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
        GamepadPref::SteamController,
        GamepadPref::SteamDeck,
        GamepadPref::DualSenseEdge,
        GamepadPref::SwitchPro,
        GamepadPref::SteamController2,
    ] {
        assert_eq!(GamepadPref::from_u8(p.to_u8()), p);
        assert_eq!(GamepadPref::from_name(p.as_str()), Some(p));
    }
    // Every wire byte 0..=9 is assigned, distinct, and pinned (forward-compat with peers
    // that only know a prefix of the range).
    for (v, p) in [
        (0, GamepadPref::Auto),
        (1, GamepadPref::Xbox360),
        (2, GamepadPref::DualSense),
        (3, GamepadPref::XboxOne),
        (4, GamepadPref::DualShock4),
        (5, GamepadPref::SteamController),
        (6, GamepadPref::SteamDeck),
        (7, GamepadPref::DualSenseEdge),
        (8, GamepadPref::SwitchPro),
        (9, GamepadPref::SteamController2),
    ] {
        assert_eq!(p.to_u8(), v);
        assert_eq!(GamepadPref::from_u8(v), p);
    }
    // The next unassigned byte degrades to Auto today; assigning it later must update this.
    assert_eq!(GamepadPref::from_u8(10), GamepadPref::Auto);
    // Aliases + unknowns.
    assert_eq!(GamepadPref::from_name("PS5"), Some(GamepadPref::DualSense));
    assert_eq!(GamepadPref::from_name("x360"), Some(GamepadPref::Xbox360));
    assert_eq!(GamepadPref::from_name("ps4"), Some(GamepadPref::DualShock4));
    assert_eq!(GamepadPref::from_name("DS4"), Some(GamepadPref::DualShock4));
    assert_eq!(
        GamepadPref::from_name("edge"),
        Some(GamepadPref::DualSenseEdge)
    );
    assert_eq!(
        GamepadPref::from_name("Switch-Pro"),
        Some(GamepadPref::SwitchPro)
    );
    assert_eq!(
        GamepadPref::from_name("ibex"),
        Some(GamepadPref::SteamController2)
    );
    assert_eq!(
        GamepadPref::from_name("sc2"),
        Some(GamepadPref::SteamController2)
    );
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
        display_hdr: None,
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
        display_hdr: None,
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
        display_hdr: None,
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
fn hello_display_hdr_roundtrip_and_back_compat() {
    let base = Hello {
        abi_version: 2,
        mode: Mode {
            width: 3840,
            height: 2160,
            refresh_hz: 120,
        },
        compositor: CompositorPref::Auto,
        gamepad: GamepadPref::Auto,
        bitrate_kbps: 0,
        name: None,
        launch: None,
        video_caps: VIDEO_CAP_10BIT | VIDEO_CAP_HDR,
        audio_channels: 2,
        video_codecs: 0,
        preferred_codec: 0,
        display_hdr: None,
    };
    // A real client-panel volume (P3 primaries, 800-nit peak, 0.05-nit floor, 400-nit FALL).
    let vol = HdrMeta {
        display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]], // G, B, R
        white_point: [15635, 16450],                                       // D65
        max_display_mastering_luminance: 8_000_000,                        // 800 nits
        min_display_mastering_luminance: 500,                              // 0.05 nits
        max_cll: 0,
        max_fall: 400,
    };
    let with_hdr = Hello {
        display_hdr: Some(vol),
        ..base.clone()
    };
    // Full roundtrip, including the forced placeholders for the earlier trailing fields.
    assert_eq!(Hello::decode(&with_hdr.encode()).unwrap(), with_hdr);
    // display_hdr alone (every earlier optional at its default) still lands at a deterministic
    // offset — the placeholder discipline holds through the whole tail.
    let hdr_only = Hello {
        video_caps: 0,
        display_hdr: Some(vol),
        ..base.clone()
    };
    assert_eq!(Hello::decode(&hdr_only.encode()).unwrap(), hdr_only);
    // An older host reading a display_hdr-bearing Hello ignores the trailing block (its decode
    // stops at preferred_codec); a new host reading an older client's Hello gets None.
    let enc = with_hdr.encode();
    assert_eq!(
        Hello::decode(&enc[..enc.len() - HDR_META_BODY_LEN]).unwrap(),
        Hello {
            display_hdr: None,
            ..with_hdr.clone()
        }
    );
    assert_eq!(Hello::decode(&base.encode()).unwrap().display_hdr, None);
    // A TRUNCATED trailing block (mid-datagram cut) degrades to None, never a partial read.
    assert_eq!(
        Hello::decode(&enc[..enc.len() - 1]).unwrap().display_hdr,
        None
    );
    // Exact wire length: 26 bitrate-era bytes + the 6 forced single-byte placeholders
    // (name len, launch len, video_caps, audio_channels, video_codecs, preferred_codec) + the body.
    assert_eq!(hdr_only.encode().len(), 26 + 6 + HDR_META_BODY_LEN);
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
fn rfi_request_roundtrip() {
    for (first_frame, last_frame) in [(0u32, 0u32), (40, 47), (5, 5), (1_000_000, u32::MAX)] {
        let r = RfiRequest {
            first_frame,
            last_frame,
        };
        assert_eq!(RfiRequest::decode(&r.encode()).unwrap(), r);
    }
    // Disjoint from the bare keyframe request (its loss-unaware sibling) and others: type byte + length.
    assert!(RfiRequest::decode(&RequestKeyframe.encode()).is_err());
    assert!(RequestKeyframe::decode(
        &RfiRequest {
            first_frame: 1,
            last_frame: 2
        }
        .encode()
    )
    .is_err());
    // Exact length — no trailing bytes.
    let bytes = RfiRequest {
        first_frame: 3,
        last_frame: 9,
    }
    .encode();
    assert!(RfiRequest::decode(&[bytes.as_slice(), &[0]].concat()).is_err());
    assert!(RfiRequest::decode(&bytes[..bytes.len() - 1]).is_err());
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
    assert_eq!(window_loss_ppm(0, 0, 0, 0), 0);
    assert_eq!(window_loss_ppm(0, 0, 1000, 0), 0);
    // 50 recovered of 1000 total (950 received + 50 recovered) = 5%.
    assert_eq!(window_loss_ppm(50, 0, 950, 0), 50_000);
    // An unrecoverable frame adds the +5% bump (push FEC past the current cap).
    assert_eq!(window_loss_ppm(50, 0, 950, 1), 100_000);
    // A total-loss window with a drop but nothing received still reports the bump, capped at 1e6.
    assert_eq!(window_loss_ppm(0, 0, 0, 3), 50_000);
    assert!(window_loss_ppm(u64::MAX, 0, 1, 9) <= 1_000_000);
    // Reordering: shards "recovered" early that then arrived are late, not lost — netted out, so
    // a pure-reorder window reads 0. Partially late nets to the true loss (20 of 1000 = 2%).
    assert_eq!(window_loss_ppm(50, 50, 1000, 0), 0);
    assert_eq!(window_loss_ppm(50, 30, 980, 0), 20_000);
    // `late` can outrun `recovered` across a window boundary (reorder straddling the report
    // tick) or via a rare wire duplicate — saturate at a clean window, never underflow.
    assert_eq!(window_loss_ppm(10, 25, 1000, 0), 0);
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
            display_hdr: None,
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
fn rumble_envelope_roundtrip_and_legacy_tolerance() {
    // v2 envelope round-trips seq + ttl.
    let d = encode_rumble_datagram_v2(2, 0x4000, 0x8000, 7, 400);
    assert_eq!(d[0], RUMBLE_MAGIC);
    assert_eq!(d.len(), RUMBLE_V2_LEN);
    assert_eq!(
        decode_rumble_envelope(&d),
        Some(RumbleUpdate {
            pad: 2,
            low: 0x4000,
            high: 0x8000,
            envelope: Some(RumbleEnvelope {
                seq: 7,
                ttl_ms: 400
            }),
        })
    );
    // The legacy level decoder reads a v2 datagram as a plain level — the tail is ignored, so an
    // old client running against a new host still renders the right amplitudes.
    assert_eq!(decode_rumble_datagram(&d), Some((2, 0x4000, 0x8000)));

    // A legacy 7-byte datagram (old host) decodes as a level with no envelope — a new client then
    // applies its own staleness policy.
    let v1 = encode_rumble_datagram(3, 0x1111, 0x2222);
    assert_eq!(
        decode_rumble_envelope(&v1),
        Some(RumbleUpdate {
            pad: 3,
            low: 0x1111,
            high: 0x2222,
            envelope: None,
        })
    );

    // A torn/short tail (8 or 9 bytes) is not a valid envelope — degrade to a level, never panic
    // or drop. (The host never emits these; a truncating middlebox might.)
    assert_eq!(
        decode_rumble_envelope(&d[..8]).map(|u| u.envelope),
        Some(None)
    );
    assert_eq!(
        decode_rumble_envelope(&d[..9]).map(|u| u.envelope),
        Some(None)
    );

    // Bad tag / too short → None on both decoders.
    assert!(decode_rumble_envelope(&d[..6]).is_none());
    let mut wrong_tag = d;
    wrong_tag[0] = AUDIO_MAGIC;
    assert!(decode_rumble_envelope(&wrong_tag).is_none());
}

#[test]
fn rumble_envelope_seq_gate_drops_reordered_stale_start() {
    use crate::input::GamepadSnapshot;
    // The client-side reorder gate (reused verbatim from gamepad snapshots): a stale start
    // arriving after a stop must not re-light the motors.
    let stop = decode_rumble_envelope(&encode_rumble_datagram_v2(0, 0, 0, 10, 0)).unwrap();
    let stale_start =
        decode_rumble_envelope(&encode_rumble_datagram_v2(0, 0x8000, 0x8000, 9, 400)).unwrap();
    let stop_seq = stop.envelope.unwrap().seq;
    let stale_seq = stale_start.envelope.unwrap().seq;
    // Nothing applied yet → the first update always passes.
    assert!(GamepadSnapshot::seq_newer(stop_seq, None));
    // The reordered older start does NOT supersede the stop.
    assert!(!GamepadSnapshot::seq_newer(stale_seq, Some(stop_seq)));
    // A genuine later renewal does.
    assert!(GamepadSnapshot::seq_newer(11, Some(stop_seq)));
    // Wraps: seq 1 supersedes 254.
    assert!(GamepadSnapshot::seq_newer(1, Some(254)));
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
    // A raw Triton state report rides the plane verbatim (as-is SC2 passthrough).
    let mut data = [0u8; HID_REPORT_MAX];
    data[0] = 0x42; // ID_TRITON_CONTROLLER_STATE
    for (i, b) in data.iter_mut().enumerate().take(46).skip(1) {
        *b = i as u8;
    }
    let raw = RichInput::HidReport {
        pad: 3,
        len: 46,
        data,
    };
    let d = raw.encode();
    assert_eq!(d.len(), 4 + 46); // tag + kind + pad + len + body — no fixed-array padding
    assert_eq!(RichInput::decode(&d), Some(raw));
    // A torn HidReport truncates to what arrived rather than over-reading (len clamps).
    assert_eq!(
        RichInput::decode(&d[..20]),
        Some(RichInput::HidReport {
            pad: 3,
            len: 16,
            data: {
                let mut t = [0u8; HID_REPORT_MAX];
                t[..16].copy_from_slice(&data[..16]);
                t
            },
        })
    );
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
        // A raw Triton rumble output report (as-is SC2 passthrough, host→client).
        HidOutput::HidRaw {
            pad: 1,
            kind: HID_RAW_OUTPUT,
            data: vec![0x80, 0, 0, 0, 0x34, 0x12, 0, 0x78, 0x56, 0],
        },
        // A raw 64-byte feature report (lizard-off / IMU-enable settings write).
        HidOutput::HidRaw {
            pad: 0,
            kind: HID_RAW_FEATURE,
            data: {
                let mut f = vec![0u8; HID_REPORT_MAX];
                f[0] = 1; // Triton feature reports ride report id 1
                f[1] = 0x87; // ID_SET_SETTINGS_VALUES
                f
            },
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
