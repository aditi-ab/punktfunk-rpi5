//! The native `punktfunk/1` handshake negotiation (plan §W1 — carved out of the [`super`] module).
//! After the pairing gate (which stays in `serve_session`, since its delegated-approval wait must
//! outlive the short handshake timeout and release the session permit), this decodes the client's
//! [`Hello`], runs mode-conflict admission, negotiates codec / compositor / gamepad / bitrate /
//! audio channels / bit-depth / chroma, reserves the data-plane UDP socket, sends the [`Welcome`],
//! and reads the client's [`Start`] — returning everything `serve_session` needs to stand the
//! session up.

use super::*;

/// Run the Hello→Welcome→Start negotiation. Borrows the control streams (the caller keeps them for
/// mid-stream renegotiation afterwards). `first` is the already-read first control message.
#[allow(clippy::type_complexity)]
pub(super) async fn negotiate(
    conn: &quinn::Connection,
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    first: &[u8],
    source: Punktfunk1Source,
    frames: u32,
    data_port: Option<u16>,
) -> Result<(
    Hello,
    Welcome,
    u16,
    std::net::UdpSocket,
    bool,
    Start,
    Option<crate::vdisplay::Compositor>,
)> {
    let peer = conn.remote_address();
    let mut hello = Hello::decode(first).map_err(|e| anyhow!("Hello decode: {e:?}"))?;
    if hello.abi_version != punktfunk_core::WIRE_VERSION {
        close_rejected(
            conn,
            punktfunk_core::reject::RejectReason::WireVersionMismatch,
        );
        anyhow::bail!(
            "wire version mismatch: client {} host {}",
            hello.abi_version,
            punktfunk_core::WIRE_VERSION
        );
    }
    // The pairing gate (require_pairing → paired? else park for delegated approval) ran above,
    // before this future, so a client reaching here is paired (or the host is `--open`).

    // Codec negotiation: pick the one codec this host will emit (its GPU-probed backend
    // capability ∩ the client's advertised codecs, honoring the client's soft preference).
    // A GPU-less software host emits H.264 only, so an HEVC-only client shares nothing with
    // it → refuse honestly rather than send a stream it can't decode.
    let host_codecs = crate::encode::Codec::host_wire_caps();
    let codec_bit =
            punktfunk_core::quic::resolve_codec(hello.video_codecs, host_codecs, hello.preferred_codec)
                .ok_or_else(|| {
                anyhow!(
                    "no shared video codec: client advertised 0x{:02x}, host can emit 0x{:02x} \
                     (a software-encode host produces H.264 — the client must advertise CODEC_H264)",
                    hello.video_codecs,
                    host_codecs
                )
            })?;
    let codec = crate::encode::Codec::from_wire(codec_bit);
    tracing::info!(
        ?codec,
        client_codecs = format_args!("0x{:02x}", hello.video_codecs),
        host_codecs = format_args!("0x{host_codecs:02x}"),
        "video codec negotiated"
    );

    // Mode-conflict ADMISSION (Stage 4): a DIFFERENT client connecting while another client's
    // session is live is resolved by the `mode_conflict` policy BEFORE the Welcome — `separate`
    // (default, no change), `join` (serve at the live mode — an honest downgrade the client
    // renders from the Welcome), `steal` (preempt the victim), or `reject` (refuse the handshake).
    // A same-client reconnect never conflicts. THIS session registers in the live set once its
    // data plane is up (below the handshake), so a later client can see + steal it.
    {
        use crate::vdisplay::admission::{admit, preempt_same_identity, Admission};
        let peer_fp = endpoint::peer_fingerprint(conn);

        // Same-client RECONNECT preempt (design §5.3 "preempts downstream"): if THIS client
        // already has a live session, it's the zombie of an unwanted disconnect whose QUIC idle
        // timer hasn't fired yet (detection lags a drop by up to `max_idle_timeout`). Signal it to
        // stop and give it the release grace so it tears its display down — which, keep-alive on,
        // lingers — and THIS reconnect REUSES that kept display below instead of landing on a
        // fresh SECOND one. Independent of the mode_conflict arm (it's our OWN prior session, not
        // a conflict with a different client), and it runs before we register ourselves so we
        // never signal our own stop flag.
        let own_zombies = preempt_same_identity(peer_fp);
        if !own_zombies.is_empty() {
            tracing::info!(
                    count = own_zombies.len(),
                    "reconnect: preempting this client's own zombie session(s) so the kept display is reused"
                );
            for z in &own_zombies {
                z.store(true, Ordering::SeqCst);
            }
            // Same blind release grace the steal path uses — lets the zombie's loops notice the
            // stop flag and drop its display (→ Lingering) before we acquire below.
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }

        match admit(peer_fp) {
            Admission::Separate => {}
            Admission::Join(m) => {
                tracing::info!(
                    requested =
                        %format_args!("{}x{}@{}", hello.mode.width, hello.mode.height, hello.mode.refresh_hz),
                    live = %format_args!("{}x{}@{}", m.0, m.1, m.2),
                    "mode-conflict: JOIN — admitting at the live display's mode"
                );
                hello.mode.width = m.0;
                hello.mode.height = m.1;
                hello.mode.refresh_hz = m.2;
            }
            Admission::Steal(victims) => {
                tracing::info!(
                    victims = victims.len(),
                    "mode-conflict: STEAL — preempting the live session(s)"
                );
                for v in &victims {
                    v.store(true, Ordering::SeqCst);
                }
                // Give the victims the release grace to tear their display down before we acquire.
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            }
            Admission::Reject(reason) => {
                tracing::warn!("mode-conflict: REJECT — {reason}");
                // Deliver the reason to the client as a TYPED refusal: close the QUIC connection
                // with the BUSY application code + the reason bytes, which the client reads from
                // the `ApplicationClosed` error (so its UI can say "host is streaming X to <name>")
                // instead of seeing a bare connection drop. Then end the handshake.
                conn.close(REJECT_BUSY_CODE.into(), reason.as_bytes());
                anyhow::bail!("{reason}");
            }
        }
    }

    crate::encode::validate_dimensions(codec, hello.mode.width, hello.mode.height)
        .context("client-requested mode")?;

    // Resolve the client's compositor preference to a concrete backend *now*, so the Welcome
    // can report what we'll actually drive. Only the Virtual source has a compositor; the
    // synthetic source has no virtual output. Blocking probes → spawn_blocking.
    let compositor = match source {
        Punktfunk1Source::Virtual => {
            let pref = hello.compositor;
            // Dedicated game session (B0): a launching client under `game_session=dedicated`
            // (gamescope available) gets its own headless gamescope spawn at the client mode. Gate on
            // whether the launch id actually RESOLVES to a command in the host's library — an unknown
            // id must fall back to normal auto routing, not a blank "sleep infinity" gamescope
            // (review #9). (dedicated is Linux-only; the resolver is the non-Windows launch_command.)
            #[cfg(not(target_os = "windows"))]
            let has_resolvable_launch = hello
                .launch
                .as_deref()
                .and_then(crate::library::launch_command)
                .is_some();
            #[cfg(target_os = "windows")]
            let has_resolvable_launch = false;
            let dedicated = crate::vdisplay::wants_dedicated_game_session(has_resolvable_launch);
            Some(
                tokio::task::spawn_blocking(move || resolve_compositor(pref, dedicated))
                    .await
                    .context("resolve compositor task")??,
            )
        }
        Punktfunk1Source::Synthetic => None,
    };

    // A requested library launch (the client sends only the store-qualified id; we look it up
    // in OUR library so a client can't inject a command) is resolved below — after the Welcome,
    // where it's threaded per-session into the data plane as `SessionContext.launch` (no
    // process-global env: the old `PUNKTFUNK_GAMESCOPE_APP` write leaked across sessions, and
    // only gamescope's bare-spawn path ever read it, so launches on every other backend were
    // silently dropped).

    // Resolve the client's gamepad-backend preference (pure env/cfg check — no probing
    // needed; the actual pads are created lazily by the input thread).
    let gamepad = resolve_gamepad(hello.gamepad);

    // Resolve the encoder bitrate (client request clamped to a sane range, or a
    // codec-aware host default — PyroWave pins ~1.6 bpp for the mode).
    let bitrate_kbps = resolve_bitrate_kbps_for(codec, hello.bitrate_kbps, &hello.mode);
    tracing::info!(
        requested_kbps = hello.bitrate_kbps,
        resolved_kbps = bitrate_kbps,
        "encoder bitrate"
    );

    // Resolve the audio channel count (client request → stereo / 5.1 / 7.1). The capturer opens
    // at this count: PipeWire synthesizes the requested positions (padding with silence when the
    // sink has fewer), WASAPI loopback up/downmixes via AUTOCONVERTPCM — so a client always gets
    // the channels it asked for, and the Welcome echoes the value the audio thread will encode.
    let audio_channels = resolve_audio_channels(hello.audio_channels);
    tracing::info!(
        requested = hello.audio_channels,
        resolved = audio_channels,
        "audio channels"
    );

    // Resolve the encode bit depth: 10-bit (HEVC Main10 / AV1 10-bit) only when ALL of — the
    // host allows it (PUNKTFUNK_10BIT, default ON with explicit-off grammar; the CLIENT's HDR
    // setting behind VIDEO_CAP_10BIT is the per-session policy switch), the client advertised
    // VIDEO_CAP_10BIT (a client that can't decode 10-bit, or an older client, always gets the
    // 8-bit stream), the codec has a 10-bit path (HEVC/AV1 — H.264 never), and the active
    // GPU/backend actually encodes 10-bit for that codec (probed, cached). Resolved BEFORE the
    // Welcome, exactly like the 4:4:4 gate below, so `color` reflects what we'll really emit —
    // the honest-downgrade channel: a GPU/backend that can't 10-bit yields 8-bit AND an SDR
    // label that matches the stream.
    let host_wants_10bit = pf_host_config::config().ten_bit;
    let client_supports_10bit = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_10BIT != 0;
    // The GPU probe may open a tiny encoder on first use, so run it off the reactor like the
    // 4:4:4 probe below (blocking probes → spawn_blocking), short-circuited behind the cheap
    // gates. The result is cached process-wide per (GPU, codec).
    let gpu_can_10bit = if host_wants_10bit && client_supports_10bit && codec.supports_10bit() {
        tokio::task::spawn_blocking(move || crate::encode::can_encode_10bit(codec))
            .await
            .context("10-bit capability probe task")?
    } else {
        false
    };
    let bit_depth: u8 = if gpu_can_10bit { 10 } else { 8 };
    tracing::info!(
        bit_depth,
        host_wants_10bit,
        client_supports_10bit,
        codec = ?codec,
        gpu_can_10bit,
        client_video_caps = hello.video_caps,
        "encode bit depth"
    );

    // Resolve the chroma subsampling: full-chroma HEVC 4:4:4 only when ALL of — the host
    // allows it (PUNKTFUNK_444, default ON; the CLIENT's 4:4:4 setting — default OFF — is the
    // per-session policy switch behind VIDEO_CAP_444), the client advertised VIDEO_CAP_444,
    // the session is single-process (the two-process WGC relay encodes 4:2:0 in v1), and the
    // active GPU/driver actually supports a 4:4:4 encode (probed, cached). The native path
    // always encodes HEVC. We resolve this BEFORE the Welcome so `chroma_format` reflects
    // what we'll really emit — the honest-downgrade channel: if any gate fails the client is
    // told 4:2:0 before it builds its decoder. The probe opens a tiny encoder; it runs only
    // when the earlier gates pass and is cached after the first.
    let host_wants_444 = pf_host_config::config().four_four_four;
    let client_supports_444 = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_444 != 0;
    // The active capturer must be able to deliver a full-chroma (RGB) source — the honest-downgrade
    // gate. Linux's portal capturer can; the Windows IDD-push path delivers subsampled NV12/P010
    // today (full-chroma IDD-push capture is a follow-up), so it returns false there and the host
    // negotiates 4:2:0. (Replaces the old `single_process` gate — single-process is now the only
    // topology, and 4:4:4 routed to DDA, which was removed.)
    let capture_supports_444 =
        crate::capture::capturer_supports_444(crate::encode::resolved_backend_ingests_rgb_444());
    // The GPU probe opens a real (tiny) encoder on first use, so run it off the reactor like the
    // compositor probe above (blocking probes → spawn_blocking). Short-circuit so it only runs when
    // the cheap gates already pass. The result is cached process-wide (a negative latches until
    // restart — acceptable: a GPU either supports HEVC 4:4:4 or it doesn't, and a transient open
    // failure here is rare since the session's own encoder isn't open yet).
    let gpu_supports_444 = if codec == crate::encode::Codec::H265
        && host_wants_444
        && client_supports_444
        && capture_supports_444
    {
        tokio::task::spawn_blocking(|| crate::encode::can_encode_444(crate::encode::Codec::H265))
            .await
            .context("4:4:4 capability probe task")?
    } else {
        false
    };
    let chroma = if gpu_supports_444 {
        crate::encode::ChromaFormat::Yuv444
    } else {
        crate::encode::ChromaFormat::Yuv420
    };
    tracing::info!(
        chroma = ?chroma,
        host_wants_444,
        client_supports_444,
        capture_supports_444,
        "encode chroma"
    );

    // Linux 4:4:4 rides the CPU swscale → 8-bit `YUV444P` path (see `encode/linux`) — there
    // is no 10-bit 4:4:4 input there, so a 10-bit-negotiated session would silently encode
    // 8-bit. Resolve the depth DOWN before the Welcome so the wire never overstates what the
    // stream carries. (Windows NVENC composes Main 4:4:4 10 from an RGB input, so it keeps
    // the resolved depth — this clamp is Linux-only.)
    #[cfg(target_os = "linux")]
    let bit_depth: u8 = if chroma.is_444() && bit_depth == 10 {
        tracing::info!("4:4:4 on the Linux path encodes 8-bit YUV444P — resolving bit depth 8");
        8
    } else {
        bit_depth
    };

    // Reserve the data-plane UDP socket up front and HOLD it through streaming (no
    // bind→read→drop→rebind window a concurrent session could race for a fixed port). A fixed
    // `--data-port` yields `direct = true` (stream straight to the client's reported address,
    // no punch-wait); otherwise a random ephemeral port + hole-punch.
    let (data_sock, direct) = bind_data_socket(data_port)?;
    let udp_port = data_sock.local_addr()?.port();

    let mut key = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut key);
    // Fresh per-session salt alongside the fresh key. GCM nonce uniqueness only *requires* one
    // of the two to be unique per session (the nonce is salt || sequence under the session
    // key), but a constant salt would make a key-reuse bug catastrophic instead of merely
    // wrong — this keeps the second line of defense real. Negotiated via Welcome, so clients
    // just follow.
    let mut salt = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut salt);
    let welcome = Welcome {
        abi_version: punktfunk_core::WIRE_VERSION,
        udp_port,
        mode: hello.mode,
        // The post-GameStream point of punktfunk/1: Leopard GF(2¹⁶) FEC + real encryption.
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            // Static override pins it; otherwise sessions start at the adaptive midpoint and the
            // host re-sizes FEC live from the client's LossReports (adaptive FEC).
            fec_percent: fec_static_override().unwrap_or(FEC_ADAPTIVE_START),
            max_data_per_block: 4096,
        },
        // The largest even payload whose sealed datagram (header + shard + crypto) fits an
        // unfragmented UDP packet on a 1500 MTU for THIS client's address family — 1408 over
        // IPv4 (1472 = the exact ceiling), 1388 over IPv6 (40-byte header, and v6 routers
        // don't fragment: overshooting there blackholes instead of degrading). The data plane
        // dials the same family as this QUIC connection, so the remote decides. The previous
        // hardcoded 1452 overshot the v4 ceiling (its math forgot the header/crypto ride
        // inside the UDP payload) and silently IP-fragmented EVERY video datagram, doubling
        // per-datagram loss on Wi-Fi — the "100 Mbps badly fails on the phone" root cause.
        // Negotiated, so the client follows. Jumbo (≈8900) is a future negotiated bump (needs
        // MAX_DATAGRAM_BYTES raised + end-to-end 9000 MTU).
        shard_payload: mtu1500_shard_payload_for(peer.ip()) as u16,
        encrypt: true,
        key,
        salt,
        frames: match source {
            Punktfunk1Source::Synthetic => frames,
            Punktfunk1Source::Virtual => 0, // unbounded — client streams until we close
        },
        // Report the resolved backends back to the client (compositor: Auto for the
        // synthetic source).
        compositor: compositor
            .map(|c| c.as_pref())
            .unwrap_or(CompositorPref::Auto),
        gamepad,
        bitrate_kbps,
        bit_depth,
        // Colour signalling the client configures its decoder/presenter from. A negotiated
        // 10-bit session is our HDR path (BT.2020 PQ — what the NVENC HEVC VUI emits from a
        // 10-bit capture format); 8-bit stays BT.709 SDR. The mastering metadata (ST.2086 +
        // CLL) rides the 0xCE datagram below. (A future step can refine this to the capturer's
        // actual monitor HDR state and announce a mid-stream flip.)
        color: if bit_depth >= 10 {
            ColorInfo::HDR10_BT2020_PQ
        } else {
            ColorInfo::SDR_BT709
        },
        // The chroma the encoder will actually emit (resolved + GPU-probed above) — 4:4:4 only
        // when every gate passed, else 4:2:0. The client sizes its decoder from this.
        chroma_format: chroma.idc(),
        // The resolved audio channel count the audio thread will capture + Opus-(multi)stream
        // encode (2/6/8). The client builds its decoder from this echoed value.
        audio_channels,
        // The negotiated codec the encoder will emit (client preference ∩ GPU capability;
        // HEVC-precedence tie-break). The client builds its decoder from this instead of
        // assuming HEVC.
        codec: codec_bit,
        // This host applies sequence-gated gamepad-state snapshots (InputKind::GamepadState),
        // so capable clients send those instead of the loss-fragile per-transition events. The
        // clipboard bit is advertised only when the operator policy enables it (design
        // clipboard-and-file-transfer.md §3.1) AND this platform has a backend — see
        // `pf_clipboard::cap_advertised` for the deliberate compositor-lacks-data-control case.
        host_caps: punktfunk_core::quic::HOST_CAP_GAMEPAD_STATE
            | if pf_clipboard::cap_advertised() {
                punktfunk_core::quic::HOST_CAP_CLIPBOARD
            } else {
                0
            },
    };
    io::write_msg(send, &welcome.encode()).await?;

    let start =
        Start::decode(&io::read_msg(recv).await?).map_err(|e| anyhow!("Start decode: {e:?}"))?;
    Ok::<_, anyhow::Error>((
        hello, welcome, udp_port, data_sock, direct, start, compositor,
    ))
}
