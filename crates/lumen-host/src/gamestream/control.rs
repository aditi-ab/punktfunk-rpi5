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
use crate::inject::InputInjector;
use anyhow::{anyhow, Context, Result};
use rusty_enet::{Event, Host, HostSettings, Packet, PeerID};
use std::net::UdpSocket;
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
        .name("lumen-control".into())
        .spawn(move || {
            // Thread-local (the injector owns non-Send Wayland/xkb state, so it must be
            // created and live here rather than be captured into the closure).
            // GCM scheme detected from the first authenticating packet; reused thereafter.
            let mut detected: Option<Scheme> = None;
            // Lazily opened on the first input event (Sway's Wayland socket is up by then).
            let mut injector: Option<Box<dyn InputInjector>> = None;
            // Virtual gamepads (uinput) + the host→client rumble sequence counter.
            let mut pads = GamepadManager::new();
            let mut rumble_seq: u32 = 0;
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
                                peer = None;
                                // Unplug the session's virtual pads.
                                pads = GamepadManager::new();
                            }
                            Event::Receive {
                                channel_id, packet, ..
                            } => {
                                on_receive(
                                    &state,
                                    channel_id,
                                    packet.data(),
                                    &mut detected,
                                    &mut injector,
                                    &mut pads,
                                );
                            }
                        },
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(error = %format!("{e:?}"), "control: service error");
                            break;
                        }
                    }
                }
                // Service the pads' force-feedback protocol every tick (games block inside
                // EVIOCSFF until answered) and relay mixed rumble levels to the client.
                if let (Some(pid), Some(scheme)) = (peer, detected) {
                    let key = state.launch.lock().unwrap().map(|s| s.gcm_key);
                    if let Some(key) = key {
                        let mut out: Vec<Vec<u8>> = Vec::new();
                        pads.pump_rumble(|index, low, high| {
                            let pt = super::gamepad::rumble_plaintext(index, low, high);
                            out.push(encrypt_control(&key, &scheme, rumble_seq, &pt));
                            rumble_seq = rumble_seq.wrapping_add(1);
                        });
                        for wire in out {
                            if let Err(e) = host.peer_mut(pid).send(0, &Packet::reliable(&wire[..]))
                            {
                                tracing::warn!(error = %format!("{e:?}"), "rumble send failed");
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

/// Handle one received control packet: decrypt it (learning the GCM scheme on the first one),
/// decode any input event, and inject it into the host session.
fn on_receive(
    state: &AppState,
    _channel_id: u8,
    d: &[u8],
    detected: &mut Option<Scheme>,
    injector: &mut Option<Box<dyn InputInjector>>,
    pads: &mut GamepadManager,
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
            pt
        }
        None => {
            tracing::warn!(len = d.len(), "control: GCM decrypt failed");
            return;
        }
    };

    // Recovery requests after loss: invalidate-reference-frames (0x0301, Gen7) or request-IDR
    // (0x0302, Gen7Enc). Force a keyframe so the client can resync without a multi-second stall.
    if pt.len() >= 2 {
        let inner = u16::from_le_bytes([pt[0], pt[1]]);
        if matches!(inner, 0x0301 | 0x0302 | 0x0305) {
            state
                .force_idr
                .store(true, std::sync::atomic::Ordering::SeqCst);
            tracing::info!(
                ty = format!("{inner:#06x}"),
                "control: IDR/RFI request → keyframe"
            );
            return;
        }
    }

    // Controller events go to the uinput virtual pads (created on demand per the mask).
    if let Some(gp) = super::gamepad::decode(&pt) {
        pads.handle(&gp);
        return;
    }

    let events = super::input::decode(&pt);
    if events.is_empty() {
        return; // keepalive / QoS / unhandled input kind
    }

    // Open the injector on demand — by the first input event the compositor session is up.
    // Backend auto-selects per desktop (wlr on Sway, libei on KWin/GNOME); override with
    // LUMEN_INPUT_BACKEND.
    if injector.is_none() {
        let backend = crate::inject::default_backend();
        match crate::inject::open(backend) {
            Ok(i) => {
                tracing::info!(?backend, "input injection backend opened");
                *injector = Some(i);
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "input injection unavailable");
                return;
            }
        }
    }
    let inj = injector.as_mut().unwrap();
    for ev in events {
        if let Err(e) = inj.inject(&ev) {
            tracing::warn!(error = %format!("{e:#}"), "inject failed");
        }
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
