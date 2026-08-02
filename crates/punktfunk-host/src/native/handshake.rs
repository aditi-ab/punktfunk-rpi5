//! The native `punktfunk/1` handshake negotiation (plan §W1 — carved out of the [`super`] module).
//! After the pairing gate (which stays in `serve_session`, since its delegated-approval wait must
//! outlive the short handshake timeout and release the session permit), this decodes the client's
//! [`Hello`], runs mode-conflict admission, negotiates codec / compositor / gamepad / bitrate /
//! audio channels / bit-depth / chroma, reserves the data-plane UDP socket, sends the [`Welcome`],
//! and reads the client's [`Start`] — returning everything `serve_session` needs to stand the
//! session up.

use super::*;

/// Whether this session forwards the cursor out-of-band (design/remote-desktop-sweep.md M2):
/// the client asked ([`CLIENT_CAP_CURSOR`](punktfunk_core::quic::CLIENT_CAP_CURSOR)) AND the
/// capture path can deliver cursor metadata separately from the frame — the Linux portal
/// `SPA_META_Cursor` path (not gamescope, whose capture paints no cursor at all), or Windows
/// with a proto-v5 pf-vdisplay driver (the IddCx hardware-cursor channel, M2c) — AND, on
/// Linux, the encode backend this session resolves to can composite the pointer on demand
/// (`encode::cursor_blend_capable`): the channel's capture-mouse flip (`CursorRenderMode`,
/// `client_draws = false`) makes the HOST draw the pointer, and on Linux the encoder is that
/// compositing stage — granting the channel over a backend that can't blend (libav
/// VAAPI/NVENC, software) shipped a cursorless stream on every capture-mode flip. Denied — or
/// never asked for (a capture-latched client, `console.rs` `latched_mouse`) — the session
/// composites host-side anyway wherever the backend can blend
/// (`session_plan::cursor_blend_for`'s no-channel arm; the compositor-EMBEDS fallback never
/// paints on a Mutter virtual stream), and only a can't-blend backend falls back to the
/// compositor EMBED. THE single predicate: the Welcome's `HOST_CAP_CURSOR` bit is computed
/// from it, and the session wiring reads that bit back.
pub(super) fn cursor_forward(
    client_caps: u8,
    compositor: Option<crate::vdisplay::Compositor>,
    codec: crate::encode::Codec,
    bit_depth: u8,
) -> bool {
    if client_caps & punktfunk_core::quic::CLIENT_CAP_CURSOR == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        // CUDA-payload prediction — the same one `SessionPlan` makes: the NVIDIA resolution
        // plus the zero-copy master switch. It decides direct-SDK NVENC (blends) vs libav
        // NVENC (doesn't) inside the capability mirror.
        let cuda_planned = !crate::encode::linux_zero_copy_is_vaapi() && crate::zerocopy::enabled();
        compositor.is_some_and(|c| c != crate::vdisplay::Compositor::Gamescope)
            && crate::encode::cursor_blend_capable(codec, cuda_planned, bit_depth == 10)
    }
    #[cfg(target_os = "windows")]
    {
        // Windows (M2c): the pf-vdisplay driver must speak the v5 hardware-cursor channel —
        // DWM composites the pointer into the IDD frame otherwise, and forwarding a second
        // copy would double it. The probe latches by opening the control device once. The
        // encoder is deliberately NOT consulted: the IDD capturer itself composites on the
        // capture-mouse flip (`set_cursor_forward`), so no Windows encode backend blends.
        let _ = (compositor, codec, bit_depth);
        crate::vdisplay::manager::hw_cursor_capable()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (compositor, codec, bit_depth);
        false
    }
}

/// Run the Hello→Welcome→Start negotiation. Borrows the control streams (the caller keeps them for
/// mid-stream renegotiation afterwards). `first` is the already-read first control message.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub(super) async fn negotiate(
    conn: &quinn::Connection,
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    first: &[u8],
    source: Punktfunk1Source,
    frames: u32,
    data_port: Option<u16>,
    // Session bring-up trace (latency plan P0.1): `welcome`/`start` stamps land here, and the
    // Welcome-time display prep threads it into the pipeline-build stages.
    bringup: &Arc<crate::bringup::Trace>,
    // The session's quit/stop flags — created BEFORE the handshake so the Welcome-time display
    // prep below can observe a client that vanished mid-handshake (its build retry aborts on
    // `stop`; `quit` rides into the display lease).
    quit: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Result<(
    Hello,
    Welcome,
    u16,
    std::net::UdpSocket,
    bool,
    Start,
    Option<crate::vdisplay::Compositor>,
    // This session's resolved gamescope sub-mode — carried to the data plane as a value rather
    // than published to process env, where a concurrent connect could overwrite it.
    Option<crate::vdisplay::GamescopeRoute>,
    Option<super::stream::PrepHandle>,
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
            // (review #9). (dedicated is Linux-only, and only there does `resolve_launch` carry a
            // command — on Windows the concrete process is resolved at launch time instead.)
            #[cfg(not(target_os = "windows"))]
            let has_resolvable_launch = hello
                .launch
                .as_deref()
                .and_then(crate::library::resolve_launch)
                .and_then(|t| t.command)
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
    // Split the pair immediately: `compositor` keeps its old meaning for the Welcome and the
    // cursor-forward decision, while the gamescope sub-mode travels separately to the data plane
    // (SessionContext → `vd.set_gamescope_route`) instead of through process env.
    let gamescope_route = compositor.as_ref().and_then(|(_, r)| r.clone());
    let compositor = compositor.map(|(c, _)| c);

    // A requested library launch (the client sends only the store-qualified id; we look it up
    // in OUR library so a client can't inject a command) is resolved below — after the Welcome,
    // where it's threaded per-session into the data plane as `SessionContext.launch` (no
    // process-global env: the old `PUNKTFUNK_GAMESCOPE_APP` write leaked across sessions, and
    // only gamescope's bare-spawn path ever read it, so launches on every other backend were
    // silently dropped).

    // Resolve the client's gamepad-backend preference (pure env/cfg check — no probing
    // needed; the actual pads are created lazily by the input thread).
    let gamepad = resolve_gamepad(hello.gamepad);

    // (The encoder bitrate is resolved below, AFTER bit depth + chroma: PyroWave's automatic
    // ~bpp pin scales with both — design/pyrowave-444-hdr.md §2.5.)

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
    // The capture side must be able to deliver a 10-bit HDR source for the NATIVE plane's
    // virtual-output capture — the honest-downgrade gate, mirroring `capturer_supports_444`.
    // SOURCE-AWARE, because on Linux the answer depends on which compositor we just resolved:
    // Windows IDD-push always can (it proactively enables advanced colour); a gamescope output
    // can when the host runs our `pipewire-hdr` gamescope build and the knob allows it; every
    // other Linux virtual output is 8-bit upstream (Mutter's RecordVirtual streams, KWin's and
    // wlroots' virtual outputs alike — GNOME 50 added HDR for *monitor* streams only, which is
    // the GameStream portal-mirror path, see `gamestream::host_hdr_capable`).
    //
    // The gamescope arm folds in its own downgrade latch
    // (`pf_capture::hdr_capture_failed(VirtualOutput)`), which is why the check the GameStream
    // path makes separately in rtsp.rs has no twin here: that latch is per-source, and this gate
    // already consulted the one belonging to the source this session will drive.
    let capture_supports_hdr = crate::capture::capturer_supports_hdr_for(compositor);
    // The GPU probe may open a tiny encoder on first use, so run it off the reactor like the
    // 4:4:4 probe below (blocking probes → spawn_blocking), short-circuited behind the cheap
    // gates. The result is cached process-wide per (GPU, codec).
    let gpu_can_10bit = if host_wants_10bit
        && client_supports_10bit
        && codec.supports_10bit()
        && capture_supports_hdr
    {
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
        capture_supports_hdr,
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
    // gate. Linux's portal capturer always can (`capturer_supports_444` returns `true`
    // unconditionally). On WINDOWS the IDD-push path CAN too, at either depth: an SDR session
    // passes the BGRA ring slot straight through and an HDR one converts the FP16 desktop to
    // packed 10-bit BT.2020 PQ RGB — both skip the subsampling converters. Only a backend that
    // ingests RGB and CSCs it to 4:4:4 itself can consume that, so the Windows arm forwards
    // `resolved_backend_ingests_rgb_444()` (today: direct-NVENC only; AMF can't 4:4:4 at all and
    // the QSV/ffmpeg path has no RGB-input 4:4:4 wiring). HDR no longer costs the chroma: 10-bit
    // 4:4:4 is HEVC Main 4:4:4 10, which is what this resolves to. (Replaces the old
    // `single_process` gate — single-process is now the only topology, and 4:4:4 routed to DDA,
    // which was removed.)
    // PyroWave does its own RGB→YCbCr CSC and its capture mode always delivers a full-chroma
    // (RGB/BGRA) source on both OSes — the capturer gate is inherently satisfied; the real
    // gate is `can_encode_444` (the full-res-chroma CSC variant existing on this OS).
    // Named for the whole capture→encoder INGEST chain, not the capturer: on Windows the
    // deciding fact is the encoder backend (direct NVENC only), and a field report burned
    // real time hunting a capture problem because the old `capture_supports_444` key said so.
    let ingest_chain_supports_444 = codec == crate::encode::Codec::PyroWave
        || crate::capture::capturer_supports_444(crate::encode::resolved_backend_ingests_rgb_444());
    // The GPU probe opens a real (tiny) encoder on first use, so run it off the reactor like the
    // compositor probe above (blocking probes → spawn_blocking). Short-circuit so it only runs when
    // the cheap gates already pass. The result is cached process-wide (a negative latches until
    // restart — acceptable: a GPU either supports HEVC 4:4:4 or it doesn't, and a transient open
    // failure here is rare since the session's own encoder isn't open yet).
    let gpu_supports_444 = if matches!(
        codec,
        crate::encode::Codec::H265 | crate::encode::Codec::PyroWave
    ) && host_wants_444
        && client_supports_444
        && ingest_chain_supports_444
    {
        tokio::task::spawn_blocking(move || crate::encode::can_encode_444(codec))
            .await
            .context("4:4:4 capability probe task")?
    } else {
        false
    };
    // The client's 4:4:4 setting IS the VIDEO_CAP_444 bit — when the user flipped it on and
    // the session still resolves 4:2:0, name the losing gate. (The PyroWave mode-size gate
    // below warns for itself.)
    if host_wants_444 && client_supports_444 && !gpu_supports_444 {
        let reason = if !matches!(
            codec,
            crate::encode::Codec::H265 | crate::encode::Codec::PyroWave
        ) {
            "the negotiated codec only carries 4:2:0 — 4:4:4 needs HEVC or PyroWave"
        } else if !ingest_chain_supports_444 {
            "this host's encoder backend can't ingest full chroma — 4:4:4 needs direct \
             NVENC (NVIDIA) or the PyroWave codec"
        } else {
            "the GPU declined the 4:4:4 encode profile probe"
        };
        tracing::info!(reason, "4:4:4 requested but the session negotiates 4:2:0");
    }
    let chroma = if gpu_supports_444 {
        crate::encode::ChromaFormat::Yuv444
    } else {
        crate::encode::ChromaFormat::Yuv420
    };
    // PyroWave-only mode-size gate: the vendored rate controller packs its block index into
    // 16 bits (pyrowave-sys patches/0002 note), which ≈8K-class 4:4:4 overflows — downgrade
    // to 4:2:0 BEFORE the Welcome (the honest-downgrade channel), like every gate above.
    let chroma = if codec == crate::encode::Codec::PyroWave
        && chroma.is_444()
        && !crate::encode::pyrowave_mode_fits_rdo(hello.mode.width, hello.mode.height, true)
    {
        tracing::warn!(
            mode = %format_args!("{}x{}", hello.mode.width, hello.mode.height),
            "PyroWave 4:4:4 at this mode exceeds the rate controller's block-index range — \
             negotiating 4:2:0"
        );
        crate::encode::ChromaFormat::Yuv420
    } else {
        chroma
    };
    tracing::info!(
        chroma = ?chroma,
        host_wants_444,
        client_supports_444,
        ingest_chain_supports_444,
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

    // Resolve the encoder bitrate (client request clamped to a sane range, or a codec-aware
    // host default). Resolved AFTER depth + chroma: PyroWave's Automatic rate is a ~bpp pin
    // for the negotiated mode that scales with both (design/pyrowave-444-hdr.md §2.5).
    let bitrate_kbps =
        resolve_bitrate_kbps_for(codec, hello.bitrate_kbps, &hello.mode, chroma, bit_depth);
    tracing::info!(
        requested_kbps = hello.bitrate_kbps,
        resolved_kbps = bitrate_kbps,
        "encoder bitrate"
    );

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
    // Session AEAD: ChaCha20-Poly1305 when the client asked for it (VIDEO_CAP_CHACHA20 — the
    // soft-AES armv7 targets, whose GCM decrypt caps at ~100 Mbps) and the operator
    // kill-switch allows (PUNKTFUNK_CHACHA20, default on — pure rollout safety; perf-only,
    // both AEADs are full-strength). The fresh-per-session discipline above applies to this
    // key identically; the legacy 16-byte `key` stays independently random so nothing
    // downstream ever observes an all-zero key.
    let client_wants_chacha = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_CHACHA20 != 0;
    let chacha = client_wants_chacha && pf_host_config::config().chacha20;
    let key_chacha = chacha.then(|| {
        let mut k = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut k);
        k
    });
    tracing::info!(
        cipher = if chacha {
            "chacha20-poly1305"
        } else {
            "aes-128-gcm"
        },
        client_wants_chacha,
        "session cipher"
    );
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
            }
            // Committed-text injection (InputKind::TextInput): only where the session's inject
            // backend can actually type text — Windows SendInput (KEYEVENTF_UNICODE) and the
            // Linux wlroots virtual keyboard (dynamic Unicode keymap). Clients without the bit
            // keep their VK-synthesis fallback for IME text.
            | if crate::inject::text_input_supported() {
                punktfunk_core::quic::HOST_CAP_TEXT_INPUT
            } else {
                0
            }
            // Cursor channel granted (client asked + this capture path can deliver cursor
            // metadata out of the frame + the resolved encoder can composite on the
            // capture-mouse flip) — the client turns its local renderer on ONLY when it sees
            // this bit, and serve_session wires forwarding by reading the bit back.
            | if cursor_forward(hello.client_caps, compositor, codec, bit_depth) {
                punktfunk_core::quic::HOST_CAP_CURSOR
            } else {
                0
            }
            // Full-fidelity stylus (0xCC/0x05 pen batches → the per-session uinput tablet):
            // Linux with /dev/uinput access, minus the PUNKTFUNK_PEN=0 kill-switch. Clients
            // without the bit keep folding pen into touch/pointer (and NativeClient::send_pen
            // refuses toward us if we don't set it).
            | if crate::inject::pen_supported() {
                punktfunk_core::quic::HOST_CAP_PEN
            } else {
                0
            },
        // The negotiated session AEAD (resolved above) + its 32-byte key toward a ChaCha
        // client; toward everyone else cipher 0 keeps the Welcome byte-identical to the
        // pre-cipher wire form. The host's own data plane picks the cipher up via
        // `welcome.session_config` — no other host change.
        cipher: if chacha {
            punktfunk_core::quic::CIPHER_CHACHA20_POLY1305
        } else {
            punktfunk_core::quic::CIPHER_AES_128_GCM
        },
        key_chacha,
    };
    io::write_msg(send, &welcome.encode()).await?;
    bringup.mark("welcome");

    // P1.1/P1.2 (latency plan): kick the display prep NOW — the negotiated mode is final in
    // the Welcome just sent, and nothing in monitor create → activation → settle → capture
    // attach → encoder open needs the client's Start or the punched socket. The prep thread
    // BECOMES the stream thread: the data plane hands it the post-punch SessionContext and it
    // runs `virtual_stream` on the warm pipeline, so the whole display bring-up hides behind
    // the Start RTT + the (up to 2.5 s) hole-punch wait. If the session dies before its data
    // plane comes up (handshake timeout, client vanished), the channel drops and the prep
    // result is released — the monitor lands in the keep-alive machinery exactly like a
    // normal session end (and `stop`, watched by the caller, aborts a still-running build
    // retry). Windows native path only: the Linux backends bind launch semantics before create
    // (gamescope nests the launch command), which must not run for a client that never sends
    // Start; GameStream has neither a Start gate nor a punch.
    #[cfg(target_os = "windows")]
    let prep: Option<super::stream::PrepHandle> = match (source, compositor) {
        (Punktfunk1Source::Virtual, Some(comp)) => {
            let (ctx_tx, ctx_rx) = std::sync::mpsc::sync_channel::<SessionContext>(1);
            let client_identity = endpoint::peer_fingerprint(conn);
            let client_hdr = hello.display_hdr;
            // The bit the Welcome just advertised — read back rather than recomputed, so the
            // prepared display and the session wiring cannot disagree with it.
            let cursor_fw = welcome.host_caps & punktfunk_core::quic::HOST_CAP_CURSOR != 0;
            // Same bit the data plane's SessionContext reads — the prepared plan and the
            // session wiring must agree on the slicing ceiling (an encoder rebuilt from the
            // prepared plan with a DIFFERENT max_slices would change the wire shape mid-flow).
            let multi_slice = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE != 0;
            let (mode, shard_payload) = (hello.mode, welcome.shard_payload);
            // "Automatic" — `bitrate_kbps` above is the host's own answer for `mode`, so the build
            // may re-resolve it if the source turns out to deliver a different size. Sampled here
            // rather than in the thread body so the closure doesn't have to capture `hello`.
            let bitrate_auto = hello.bitrate_kbps == 0;
            let trace = bringup.clone();
            std::thread::Builder::new()
                .name("punktfunk1-stream".into())
                .spawn(move || -> Result<()> {
                    let prepared = super::stream::prepare_display(
                        comp,
                        mode,
                        client_identity,
                        client_hdr,
                        cursor_fw,
                        multi_slice,
                        bitrate_kbps,
                        bitrate_auto,
                        bit_depth,
                        chroma,
                        codec,
                        shard_payload,
                        &quit,
                        &stop,
                        &trace,
                    );
                    let Ok(ctx) = ctx_rx.recv() else {
                        // No data plane ever came (handshake abort / punch failure): drop
                        // `prepared` — its lease release hands the monitor to keep-alive
                        // policy, exactly like a normal session end.
                        return Ok(());
                    };
                    match prepared {
                        Ok(p) => virtual_stream(ctx, Some(p)),
                        Err(e) => Err(e),
                    }
                })
                .map(|handle| (ctx_tx, handle))
                .map_err(|e| {
                    tracing::warn!(error = %e,
                        "display-prep thread spawn failed — falling back to inline bring-up")
                })
                .ok()
        }
        _ => None,
    };
    #[cfg(not(target_os = "windows"))]
    let prep: Option<super::stream::PrepHandle> = None;
    #[cfg(not(target_os = "windows"))]
    let _ = (quit, stop);

    let start =
        Start::decode(&io::read_msg(recv).await?).map_err(|e| anyhow!("Start decode: {e:?}"))?;
    bringup.mark("start");
    Ok::<_, anyhow::Error>((
        hello,
        welcome,
        udp_port,
        data_sock,
        direct,
        start,
        compositor,
        gamescope_route,
        prep,
    ))
}
