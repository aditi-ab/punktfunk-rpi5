//! The GameStream control stream: an ENet host on UDP 47999. Moonlight connects this
//! BEFORE the video stream starts (`STAGE_CONTROL_STREAM_START` precedes
//! `STAGE_VIDEO_STREAM_START`), so it must be up or the whole connection aborts. It carries
//! input (mouse/keyboard/gamepad), keepalives, and QoS feedback.
//!
//! Sunshine-mode hosts (we advertise `state=SUNSHINE_SERVER_FREE`) make Moonlight encrypt the
//! control stream with AES-128-GCM under the `/launch` `rikey`, even though we negotiate no
//! media encryption. Wire framing (all little-endian):
//!
//! ```text
//!   u16 encType = 0x0001 | u16 length | u32 seq | [16-byte GCM tag] | ciphertext
//!   length = sizeof(seq) + 16 (tag) + plaintext
//! ```
//!
//! The GCM nonce depends on what Moonlight negotiated (`encryptControlMessage` in
//! moonlight-common-c). For `SS_ENC_CONTROL_V2` it is a 12-byte nonce with `seq` (LE) in bytes
//! [0..4] and `b"CC"` (client→host) at [10..12]. For the legacy path — which we hit, since we
//! advertise no encryption — it is a 16-byte nonce with only `iv[0] = seq & 0xff` and the rest
//! zero. The tag is prepended to the ciphertext; there is no AAD; the key is the forward
//! `hex::decode(rikey)`. We auto-detect the exact scheme via [`decrypt_control`] on the first
//! packet that authenticates, since GCM gives no partial credit.
//!
//! Runs on its own native thread for the host's lifetime.

use super::{AppState, CONTROL_PORT};
use crate::inject::gamepad::GamepadManager;
use anyhow::{anyhow, Context, Result};
use punktfunk_core::input::InputEvent;
use punktfunk_core::quic::HdrMeta;
use rusty_enet::{Event, Host, HostSettings, Packet, PeerID};
use std::net::UdpSocket;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

/// Bind the ENet control host on 47999 and service it forever on a dedicated thread.
pub fn spawn(state: Arc<AppState>) -> Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", CONTROL_PORT)).context("bind control UDP")?;
    socket
        .set_nonblocking(true)
        .context("control socket nonblocking")?;
    let mut host = Host::new(
        socket,
        HostSettings {
            peer_limit: 4,
            // Moonlight connects with CTRL_CHANNEL_COUNT (0x30) channels and sends gamepad
            // input on channel 0x10+n — a smaller limit silently discards controller input.
            channel_limit: 0x30,
            ..Default::default()
        },
    )
    .map_err(|e| anyhow!("ENet host init: {e:?}"))?;
    tracing::info!(port = CONTROL_PORT, "ENet control listening");

    std::thread::Builder::new()
        .name("punktfunk-control".into())
        .spawn(move || {
            // GCM scheme detected from the first authenticating packet; reused thereafter.
            let mut detected: Option<Scheme> = None;
            // Consecutive control-decrypt failures for this peer — throttles the warn log so a
            // junk-packet flood can't spam unbounded lines (security-review 2026-06-28 #10).
            let mut decrypt_fails: u64 = 0;
            // Decoded keyboard/mouse is forwarded to a dedicated host-lifetime injector thread —
            // NEVER injected inline, so a slow Wayland/libei/SendInput call can't head-block ENet
            // keepalive/retransmit servicing on this thread. The injector owns non-Send compositor
            // state and lives on its own thread (see crate::inject::InjectorService); the held
            // `inj_tx` clone keeps it alive for the control thread's lifetime.
            let inj_tx = crate::inject::InjectorService::start().sender();
            // Virtual gamepads (uinput). ONE monotonic host→client control sequence counter, shared
            // by every outbound message (rumble + the HDR-mode signal): the GCM nonce is derived
            // from `seq`, so a per-message-type counter would reuse (key, nonce) pairs across
            // message types in the host direction.
            let mut pads = GamepadManager::new();
            // Pen/touch translator (SS_PEN/SS_TOUCH → virtual tablet / wire touch). Sent only
            // by clients that saw our SS_FF_PEN_TOUCH_EVENTS feature flag (rtsp.rs).
            let mut pointer = super::pen::GsPointer::new();
            let mut host_seq: u32 = 0;
            // One-shot latch for the HDR-mode control message (0x010e); re-armed on Disconnect.
            let mut hdr_sent = false;
            let mut peer: Option<PeerID> = None;
            loop {
                loop {
                    match host.service() {
                        Ok(Some(event)) => match event {
                            Event::Connect { peer: p, .. } => {
                                tracing::info!("control: client connected");
                                peer = Some(p.id());
                            }
                            Event::Disconnect { .. } => {
                                tracing::info!("control: client disconnected");
                                detected = None;
                                decrypt_fails = 0;
                                peer = None;
                                // Re-arm the HDR-mode signal for the next connection.
                                hdr_sent = false;
                                // Unplug the session's virtual pads + tablet (destroying the
                                // uinput pen releases any held tool/tip kernel-side).
                                pads = GamepadManager::new();
                                pointer = super::pen::GsPointer::new();
                            }
                            Event::Receive {
                                channel_id, packet, ..
                            } => {
                                on_receive(
                                    &state,
                                    channel_id,
                                    packet.data(),
                                    &mut detected,
                                    &mut decrypt_fails,
                                    &inj_tx,
                                    &mut pads,
                                    &mut pointer,
                                );
                            }
                        },
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(error = ?e, "control: service error");
                            break;
                        }
                    }
                }
                // Service the pads' force-feedback protocol every tick (games block inside
                // EVIOCSFF until answered) and relay mixed rumble levels to the client.
                //
                // SECURITY NOTE (audit #5, legacy GCM nonce reuse): on the LEGACY control scheme
                // (`NonceKind::Legacy*`, which we hit because we advertise no encryption) the nonce is
                // just the per-direction `seq` (`iv[0]=seq&0xff`, rest zero) with NO direction byte —
                // so host control messages (this `host_seq`, shared by rumble + the HDR-mode signal)
                // and client input (its own seq) share the same (key, nonce) space when their seqs
                // collide. This is INHERENT to Nvidia's old-style
                // GameStream control encryption (Apollo/moonlight-common-c are identical: only the V2
                // scheme adds `iv[10..12] = 'H','C'` to separate the host direction). It can't be fixed
                // on the legacy wire without breaking Moonlight; the GCM key is the client-supplied
                // `rikey` (so only a passive eavesdropper who missed the HTTPS /launch is the
                // adversary). The real fix is V2 control-encryption negotiation; for untrusted networks
                // use the native punktfunk/1 plane (correct per-direction nonces + seq-as-AAD).
                if let (Some(pid), Some(scheme)) = (peer, detected) {
                    let key = state.launch.lock().unwrap().map(|s| s.gcm_key);
                    if let Some(key) = key {
                        let mut out: Vec<Vec<u8>> = Vec::new();
                        // One-shot HDR-mode signal (type 0x010e / Sunshine `IDX_HDR_MODE`) once the
                        // control stream is live. Stock Moonlight clients only flip the TV into HDR
                        // picture mode when they receive this async message — the video is already
                        // BT.2020 PQ, but without the cue the display stays in SDR mode (the exact
                        // symptom aurora-tv PR #53 worked around client-side). Sent before rumble so
                        // the client sees HDR as early as possible.
                        if !hdr_sent {
                            // `state.stream` is populated by the RTSP ANNOUNCE, which precedes the
                            // control stream — but guard the race: only commit the one-shot decision
                            // once a config exists, so a not-yet-set stream can't latch us into never
                            // signaling HDR. A non-HDR session latches too (it never needs the msg).
                            if let Some(hdr) = state.stream.lock().unwrap().map(|s| s.hdr) {
                                if hdr {
                                    let pt =
                                        hdr_mode_plaintext(true, &pf_frame::hdr::generic_hdr10());
                                    out.push(encrypt_control(&key, &scheme, host_seq, &pt));
                                    host_seq = host_seq.wrapping_add(1);
                                    tracing::info!(
                                        "control: signaled HDR mode ON to client (0x010e)"
                                    );
                                }
                                hdr_sent = true;
                            }
                        }
                        pads.pump_rumble(|index, low, high| {
                            let pt = super::gamepad::rumble_plaintext(index, low, high);
                            out.push(encrypt_control(&key, &scheme, host_seq, &pt));
                            host_seq = host_seq.wrapping_add(1);
                        });
                        for wire in out {
                            if let Err(e) = host.peer_mut(pid).send(0, &Packet::reliable(&wire[..]))
                            {
                                tracing::warn!(error = ?e, "control send failed");
                            }
                        }
                    }
                } else {
                    // No client/scheme yet: still answer FF uploads so games don't block.
                    pads.pump_rumble(|_, _, _| {});
                }
                // ENet needs frequent servicing for handshake/keepalive/retransmit.
                std::thread::sleep(Duration::from_millis(2));
            }
        })
        .context("spawn control thread")?;
    Ok(())
}

/// Decode the lost-frame range from an invalidate-reference-frames (0x0301) control message: two
/// little-endian `i64` (firstFrame, lastFrame) after the 4-byte `[u16 type][u16 length]` header,
/// matching Sunshine/Apollo's `IDX_INVALIDATE_REF_FRAMES`. Returns `None` when the body is too
/// short or the range is nonsensical, in which case the caller falls back to a full IDR.
fn decode_rfi_range(pt: &[u8]) -> Option<(i64, i64)> {
    if pt.len() < 20 {
        return None;
    }
    let first = i64::from_le_bytes(pt[4..12].try_into().ok()?);
    let last = i64::from_le_bytes(pt[12..20].try_into().ok()?);
    (first >= 0 && last >= first).then_some((first, last))
}

/// Handle one received control packet: decrypt it (learning the GCM scheme on the first one),
/// decode any input event, and inject it into the host session.
#[allow(clippy::too_many_arguments)]
fn on_receive(
    state: &AppState,
    _channel_id: u8,
    d: &[u8],
    detected: &mut Option<Scheme>,
    decrypt_fails: &mut u64,
    inj_tx: &Sender<InputEvent>,
    pads: &mut GamepadManager,
    pointer: &mut super::pen::GsPointer,
) {
    let Some(key) = state.launch.lock().unwrap().map(|s| s.gcm_key) else {
        return; // control traffic before /launch — no key yet
    };
    // Encrypted control packets begin with u16 LE encType = 0x0001 and an 8-byte header.
    if d.len() < 8 || d[0] != 0x01 || d[1] != 0x00 {
        return;
    }

    let pt = match decrypt_control(&key, d, detected) {
        Some((scheme, pt)) => {
            if detected.is_none() {
                tracing::info!(?scheme, "control: GCM scheme locked in");
            }
            *detected = Some(scheme);
            *decrypt_fails = 0;
            pt
        }
        None => {
            // Throttle: a junk-packet flood must not spam one warn line per packet. Log the first
            // failure, then only at exponentially-spaced counts (1, 2, 4, 8, …).
            *decrypt_fails += 1;
            if decrypt_fails.is_power_of_two() {
                tracing::warn!(
                    len = d.len(),
                    fails = *decrypt_fails,
                    "control: GCM decrypt failed"
                );
            }
            return;
        }
    };

    // Recovery requests after loss. Invalidate-reference-frames (0x0301, Gen7) carries the lost
    // frame range (two LE i64 after the [type][len] header, like Sunshine/Apollo's
    // IDX_INVALIDATE_REF_FRAMES) — route it to the encoder, which invalidates those refs instead of
    // a full IDR when it can (NVENC RFI). Request-IDR (0x0302 / 0x0305) and a malformed 0x0301 force
    // a keyframe. The video thread drains rfi_range/force_idr and resyncs without a multi-second stall.
    if pt.len() >= 2 {
        let inner = u16::from_le_bytes([pt[0], pt[1]]);
        if inner == 0x0301 {
            if let Some((first, last)) = decode_rfi_range(&pt) {
                *state.rfi_range.lock().unwrap() = Some((first, last));
                tracing::debug!(first, last, "control: RFI request → invalidate ref frames");
            } else {
                state
                    .force_idr
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                tracing::debug!("control: RFI request (no range) → keyframe");
            }
            return;
        }
        if matches!(inner, 0x0302 | 0x0305) {
            state
                .force_idr
                .store(true, std::sync::atomic::Ordering::SeqCst);
            tracing::debug!(
                ty = %format_args!("{inner:#06x}"),
                "control: IDR request → keyframe"
            );
            return;
        }
    }

    // Controller events go to the uinput virtual pads (created on demand per the mask).
    if let Some(gp) = super::gamepad::decode(&pt) {
        pads.handle(&gp);
        return;
    }

    // Pen/touch extension events (Moonlight sends them only after seeing our feature flag):
    // pen drives this session's virtual tablet; touch forwards as ordinary wire touches.
    if let Some(p) = super::input::decode_pointer(&pt) {
        pointer.apply(&p, |ev| {
            let _ = inj_tx.send(ev);
        });
        return;
    } else if super::input::is_pointer_magic(&pt) {
        // A pointer magic that failed the body parse — a layout mismatch against this
        // client, exactly what an on-glass "touch/pen does nothing" needs surfaced. The
        // first few dump their bytes so the mismatch is diagnosable from the log alone.
        static HEX_DUMPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if HEX_DUMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 5 {
            let hex: String = pt.iter().map(|b| format!("{b:02x}")).collect();
            tracing::warn!(
                len = pt.len(),
                bytes = %hex,
                "gamestream: SS_TOUCH/SS_PEN packet failed to decode (malformed/unexpected layout)"
            );
        } else {
            tracing::warn!(
                len = pt.len(),
                "gamestream: SS_TOUCH/SS_PEN packet failed to decode (malformed/unexpected layout)"
            );
        }
        return;
    }

    let events = super::input::decode(&pt);
    if events.is_empty() {
        return; // keepalive / QoS / unhandled input kind
    }

    // Forward to the dedicated injector thread (it opens the backend on the first event and
    // coalesces redundant motion). A closed channel means the injector thread died at startup —
    // input is lossy, so drop silently rather than spam.
    for ev in events {
        let _ = inj_tx.send(ev);
    }
}

/// How a control packet's nonce is built — Moonlight picks one based on the negotiated flags.
#[derive(Clone, Copy, Debug)]
enum NonceKind {
    /// `SS_ENC_CONTROL_V2`: 12-byte nonce, `seq` in [0..4], marker bytes at [10..12].
    V2 { seq_be: bool, marker: [u8; 2] },
    /// Legacy: 16-byte nonce, only `iv[0] = seq & 0xff` (the rest zero).
    LegacyLowByte,
    /// Legacy variant: 16-byte nonce, full `seq` in [0..4] (the rest zero).
    Legacy16Seq { seq_be: bool },
}

impl NonceKind {
    fn nonce(&self, seq: u32) -> Vec<u8> {
        let seq_bytes = |be: bool| {
            if be {
                seq.to_be_bytes()
            } else {
                seq.to_le_bytes()
            }
        };
        match *self {
            NonceKind::V2 { seq_be, marker } => {
                let mut iv = vec![0u8; 12];
                iv[0..4].copy_from_slice(&seq_bytes(seq_be));
                iv[10] = marker[0];
                iv[11] = marker[1];
                iv
            }
            NonceKind::LegacyLowByte => {
                let mut iv = vec![0u8; 16];
                iv[0] = (seq & 0xff) as u8;
                iv
            }
            NonceKind::Legacy16Seq { seq_be } => {
                let mut iv = vec![0u8; 16];
                iv[0..4].copy_from_slice(&seq_bytes(seq_be));
                iv
            }
        }
    }
}

/// The byte-exact GCM scheme that opened a control packet. Determined empirically once per
/// connection (AES-GCM gives no partial credit, so an authenticating combination is proof).
#[derive(Clone, Copy, Debug)]
struct Scheme {
    /// `gcm_key` is byte-reversed before use (defensive; Sunshine's net effect is forward).
    key_rev: bool,
    nonce: NonceKind,
    /// GCM tag sits before the ciphertext (vs after).
    tag_first: bool,
    aad: Aad,
}

#[derive(Clone, Copy, Debug)]
enum Aad {
    None,
    /// The 4-byte cleartext header prefix (encType + length), `d[0..4]`.
    Header4,
}

impl Scheme {
    fn key(&self, base: &[u8; 16]) -> [u8; 16] {
        let mut k = *base;
        if self.key_rev {
            k.reverse();
        }
        k
    }
}

/// Open an encrypted control packet `d` (8-byte cleartext header + `[tag?][ciphertext]`). If
/// `detected` is set only that scheme is tried (fast path); otherwise the full cross-product
/// of plausible schemes (nonce construction × key byte-order × tag position × AAD) is swept
/// and the combination whose GCM tag authenticates is returned.
fn decrypt_control(
    key: &[u8; 16],
    d: &[u8],
    detected: &Option<Scheme>,
) -> Option<(Scheme, Vec<u8>)> {
    let seq = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
    let payload = &d[8..];
    if payload.len() < 16 {
        return None;
    }

    let attempt = |s: Scheme| -> Option<Vec<u8>> {
        // aes-gcm wants `ciphertext || tag`; reassemble from whichever wire order this is.
        let (ct, tag) = if s.tag_first {
            (&payload[16..], &payload[..16])
        } else {
            (
                &payload[..payload.len() - 16],
                &payload[payload.len() - 16..],
            )
        };
        let mut ct_tag = Vec::with_capacity(ct.len() + 16);
        ct_tag.extend_from_slice(ct);
        ct_tag.extend_from_slice(tag);
        let aad: &[u8] = match s.aad {
            Aad::None => &[],
            Aad::Header4 => &d[0..4],
        };
        gcm_open(&s.key(key), &s.nonce.nonce(seq), &ct_tag, aad)
    };

    if let Some(s) = *detected {
        return attempt(s).map(|pt| (s, pt));
    }

    // Candidate nonce constructions, most-likely first.
    const MARKERS: [[u8; 2]; 3] = [*b"CC", *b"HC", *b"CH"];
    let mut kinds: Vec<NonceKind> = vec![NonceKind::LegacyLowByte];
    for seq_be in [false, true] {
        for marker in MARKERS {
            kinds.push(NonceKind::V2 { seq_be, marker });
        }
        kinds.push(NonceKind::Legacy16Seq { seq_be });
    }

    for &nonce in &kinds {
        for key_rev in [false, true] {
            for tag_first in [true, false] {
                for aad in [Aad::None, Aad::Header4] {
                    let s = Scheme {
                        key_rev,
                        nonce,
                        tag_first,
                        aad,
                    };
                    if let Some(pt) = attempt(s) {
                        return Some((s, pt));
                    }
                }
            }
        }
    }
    None
}

/// Serialize an [`HdrMeta`] into Moonlight's `SS_HDR_METADATA` control-message layout: 26 bytes,
/// all **little-endian** (moonlight-common-c parses it with `BYTE_ORDER_LITTLE`), primaries in
/// **R, G, B** order — note [`HdrMeta`]/ST.2086 stores them G, B, R, so we reorder. Luminance is
/// re-scaled to the wire units the client reads: `maxDisplayLuminance`/`maxFullFrameLuminance` in
/// whole nits, `minDisplayLuminance` in 1/10000-nit, content light levels already in nits. There is
/// no separate full-frame luminance in our metadata, so it mirrors the mastering peak.
fn ss_hdr_metadata(m: &HdrMeta) -> [u8; 26] {
    let max_display_nits = (m.max_display_mastering_luminance / 10_000).min(u16::MAX as u32) as u16;
    let min_display = m.min_display_mastering_luminance.min(u16::MAX as u32) as u16;
    let mut b = [0u8; 26];
    let mut o = 0;
    let mut put = |v: u16| {
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
        o += 2;
    };
    // displayPrimaries[3] in R, G, B order (HdrMeta is G, B, R).
    for p in [
        m.display_primaries[2],
        m.display_primaries[0],
        m.display_primaries[1],
    ] {
        put(p[0]);
        put(p[1]);
    }
    put(m.white_point[0]);
    put(m.white_point[1]);
    put(max_display_nits); // maxDisplayLuminance (nits)
    put(min_display); // minDisplayLuminance (1/10000 nit)
    put(m.max_cll); // maxContentLightLevel (nits)
    put(m.max_fall); // maxFrameAverageLightLevel (nits)
    put(max_display_nits); // maxFullFrameLuminance (nits) — no separate value; mirror the peak
    debug_assert_eq!(o, 26);
    b
}

/// Build the host→client HDR-mode control plaintext (type `0x010e` / Sunshine `IDX_HDR_MODE`):
/// `[u16 type][u16 length][u8 enabled][SS_HDR_METADATA]`, all little-endian, `length` counting the
/// enable byte + metadata (mirrors [`super::gamepad::rumble_plaintext`]). Moonlight flips the
/// TV/decoder into HDR picture mode on `enabled != 0` (`ConnListenerSetHdrMode`). We advertise a
/// Sunshine server, so the client (`IS_SUNSHINE()`) reads the full 26-byte metadata block.
fn hdr_mode_plaintext(enabled: bool, m: &HdrMeta) -> Vec<u8> {
    let meta = ss_hdr_metadata(m);
    let mut pt = Vec::with_capacity(4 + 1 + meta.len());
    pt.extend_from_slice(&0x010eu16.to_le_bytes()); // type
    pt.extend_from_slice(&((1 + meta.len()) as u16).to_le_bytes()); // length = enable + metadata
    pt.push(enabled as u8);
    pt.extend_from_slice(&meta);
    pt
}

/// Seal a host→client control message, mirroring the client's `detected` scheme with the
/// direction flipped: V2 nonces use marker `H?` (host-originated) instead of `C?`; legacy
/// nonces keep their construction with our own independent `seq` counter. Wire layout matches
/// what the client sends us: `[0x0001][length][seq][tag|ct per scheme.tag_first]`.
fn encrypt_control(key: &[u8; 16], scheme: &Scheme, seq: u32, pt: &[u8]) -> Vec<u8> {
    let nonce_kind = match scheme.nonce {
        NonceKind::V2 { seq_be, marker } => NonceKind::V2 {
            seq_be,
            marker: [b'H', marker[1]],
        },
        other => other,
    };
    let length = (4 + 16 + pt.len()) as u16;
    let mut wire = Vec::with_capacity(8 + 16 + pt.len());
    wire.extend_from_slice(&0x0001u16.to_le_bytes());
    wire.extend_from_slice(&length.to_le_bytes());
    wire.extend_from_slice(&seq.to_le_bytes());
    let aad: Vec<u8> = match scheme.aad {
        Aad::None => Vec::new(),
        Aad::Header4 => wire[0..4].to_vec(),
    };
    let ct_tag = gcm_seal(&scheme.key(key), &nonce_kind.nonce(seq), pt, &aad);
    let (ct, tag) = ct_tag.split_at(ct_tag.len() - 16);
    if scheme.tag_first {
        wire.extend_from_slice(tag);
        wire.extend_from_slice(ct);
    } else {
        wire.extend_from_slice(ct);
        wire.extend_from_slice(tag);
    }
    wire
}

/// AES-128-GCM seal (companion to [`gcm_open`]); returns `ciphertext || tag`.
fn gcm_seal(key: &[u8; 16], nonce: &[u8], pt: &[u8], aad: &[u8]) -> Vec<u8> {
    use aes_gcm::aead::consts::{U12, U16};
    use aes_gcm::aead::generic_array::GenericArray;
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{aes::Aes128, AesGcm};

    let p = Payload { msg: pt, aad };
    match nonce.len() {
        12 => AesGcm::<Aes128, U12>::new_from_slice(key)
            .unwrap()
            .encrypt(GenericArray::from_slice(nonce), p)
            .expect("GCM seal"),
        16 => AesGcm::<Aes128, U16>::new_from_slice(key)
            .unwrap()
            .encrypt(GenericArray::from_slice(nonce), p)
            .expect("GCM seal"),
        _ => unreachable!("nonce length"),
    }
}

/// AES-128-GCM open with a 12- or 16-byte nonce and explicit AAD. Returns the plaintext iff
/// the tag authenticates. `ct_tag` is `ciphertext || tag` (aes-gcm's expected order).
fn gcm_open(key: &[u8; 16], nonce: &[u8], ct_tag: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::aead::consts::{U12, U16};
    use aes_gcm::aead::generic_array::GenericArray;
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{aes::Aes128, AesGcm};

    let p = Payload { msg: ct_tag, aad };
    match nonce.len() {
        12 => AesGcm::<Aes128, U12>::new_from_slice(key)
            .ok()?
            .decrypt(GenericArray::from_slice(nonce), p)
            .ok(),
        16 => AesGcm::<Aes128, U16>::new_from_slice(key)
            .ok()?
            .decrypt(GenericArray::from_slice(nonce), p)
            .ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::decode_rfi_range;

    /// Build a 0x0301 invalidate-ref-frames plaintext: `[type LE][len LE][firstFrame i64 LE][last i64 LE]`.
    fn rfi_msg(first: i64, last: i64) -> Vec<u8> {
        let mut v = vec![0x01, 0x03, 0x10, 0x00]; // type 0x0301, length 16
        v.extend_from_slice(&first.to_le_bytes());
        v.extend_from_slice(&last.to_le_bytes());
        v
    }

    #[test]
    fn decodes_a_valid_rfi_range() {
        assert_eq!(decode_rfi_range(&rfi_msg(40, 47)), Some((40, 47)));
        assert_eq!(decode_rfi_range(&rfi_msg(5, 5)), Some((5, 5))); // single frame
    }

    #[test]
    fn rejects_short_or_nonsensical_ranges() {
        assert_eq!(decode_rfi_range(&[0x01, 0x03, 0x00, 0x00]), None); // header only, no body
        assert_eq!(decode_rfi_range(&rfi_msg(-1, 9)), None); // negative first
        assert_eq!(decode_rfi_range(&rfi_msg(9, 4)), None); // last < first
    }

    /// The HDR-mode plaintext must match what moonlight-common-c parses: `[u16 type=0x010e]
    /// [u16 length=27][u8 enable][SS_HDR_METADATA]`, 31 bytes, all little-endian, primaries R,G,B.
    #[test]
    fn hdr_mode_plaintext_wire_layout() {
        let pt = super::hdr_mode_plaintext(true, &pf_frame::hdr::generic_hdr10());
        assert_eq!(pt.len(), 31); // 4 header + 1 enable + 26 metadata
        assert_eq!(&pt[0..2], &0x010eu16.to_le_bytes()); // type
        assert_eq!(&pt[2..4], &27u16.to_le_bytes()); // length = enable + metadata
        assert_eq!(pt[4], 1); // enabled
                              // Metadata starts at byte 5, R primary first (HdrMeta stores G,B,R; wire is R,G,B).
        assert_eq!(&pt[5..7], &35400u16.to_le_bytes()); // red.x
        assert_eq!(&pt[7..9], &14600u16.to_le_bytes()); // red.y
        assert_eq!(&pt[9..11], &8500u16.to_le_bytes()); // green.x
        assert_eq!(&pt[13..15], &6550u16.to_le_bytes()); // blue.x
        assert_eq!(&pt[17..19], &15635u16.to_le_bytes()); // whitePoint.x
        assert_eq!(&pt[21..23], &1000u16.to_le_bytes()); // maxDisplayLuminance (nits)
        assert_eq!(&pt[23..25], &1u16.to_le_bytes()); // minDisplayLuminance (1/10000 nit)
        assert_eq!(&pt[25..27], &1000u16.to_le_bytes()); // maxContentLightLevel (MaxCLL)
        assert_eq!(&pt[27..29], &400u16.to_le_bytes()); // maxFrameAverageLightLevel (MaxFALL)
        assert_eq!(&pt[29..31], &1000u16.to_le_bytes()); // maxFullFrameLuminance mirrors the peak
    }

    #[test]
    fn hdr_mode_plaintext_disabled_still_well_formed() {
        let pt = super::hdr_mode_plaintext(false, &pf_frame::hdr::generic_hdr10());
        assert_eq!(pt.len(), 31);
        assert_eq!(&pt[2..4], &27u16.to_le_bytes());
        assert_eq!(pt[4], 0); // disabled
    }
}
