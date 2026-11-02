//! A 2015 Steam Controller on Bluetooth, folded into the wired frames its cable and dongle send.
//! Every capture turns the pad's Bluetooth input into `ID_CONTROLLER_STATE` frames here, so the
//! gate, the host's `28DE:1102` mirror and each client's parser see one format. The host's
//! feature writes go back as segments, and feature replies come in as segments.
//!
//! The pad speaks in 20-byte segments, `[0x03, header, 18 bytes]`: the header's `0x80` marks
//! data, `0x40` the last segment, its low three bits the segment number. A state message carries
//! only the chunks that changed, so [`classic_ble_input`] keeps the rest in a state the caller
//! holds (SDL `SDL_hidapi_steam.c`, `UpdateBLESteamControllerState`).

/// The report id every segment carries.
const REPORT: u8 = 0x03;
/// One segment, report id included.
pub const SEGMENT: usize = 20;
const PAYLOAD: usize = 18;
const DATA: u8 = 0x80;
const LAST: u8 = 0x40;
/// Four segments carry a 64-byte feature frame and the longest state message (42 bytes).
const MAX_SEGMENTS: usize = 4;
const MESSAGE: usize = MAX_SEGMENTS * PAYLOAD;

/// The chunked state message: this in the low nibble of its first byte (`k_EBLEReportState`).
const CHUNKED_STATE: u8 = 4;
/// The older state message, a wired-style frame with one IMU block (`ID_CONTROLLER_BLE_STATE`).
const TYPE_BLE_STATE: u8 = 7;

/// Byte 10 of the wired frame: a finger on the left pad, and the stick in use meanwhile. With
/// both, the frame's bytes 16..20 hold the pad when the first is set and the stick when not.
const LPAD_TOUCH: u8 = 0x08;
const LPAD_AND_STICK: u8 = 0x80;

/// Bytes of state one pad's input keeps between segments; the caller zeroes it once per link.
pub const CLASSIC_BLE_STATE: usize = 160;

// The state: the frame being built, the pad's own bytes 8..11, the left pad and the stick (they
// take turns in the frame), flags, then the segment assembler.
const S_BUTTONS: usize = 64;
const S_STICK: usize = 67;
const S_LPAD: usize = 71;
const S_FLAGS: usize = 75;
const S_NEXT: usize = 76;
const S_BUF: usize = 77;
const STARTED: u8 = 1;
const STICK_TURN: u8 = 2;
const _: () = assert!(S_BUF + MESSAGE <= CLASSIC_BLE_STATE);

/// Chunks of a chunked state message, in wire order: (mask bit, length, where it lands).
const CHUNKS: [(u16, usize, usize); 9] = [
    (0x0010, 3, S_BUTTONS),
    (0x0020, 2, 11), // the u8 triggers
    (0x0040, 3, 13),
    (0x0080, 4, S_STICK),
    (0x0100, 4, S_LPAD),
    (0x0200, 4, 20), // the right pad
    (0x0400, 6, 28), // accelerometer
    (0x0800, 6, 34), // gyro
    (0x1000, 8, 40), // quaternion
];

/// Fold `seg` into the assembler; the message length once its last segment is in. A segment out
/// of order restarts the message, as SDL's assembler does.
fn assemble(next: &mut u8, buf: &mut [u8], seg: &[u8]) -> Option<usize> {
    let [REPORT, header, payload @ ..] = seg else {
        return None;
    };
    if payload.len() < PAYLOAD || header & DATA == 0 {
        return None; // the pad sends empty segments now and then
    }
    let n = usize::from(header & 0x07);
    if n != usize::from(*next) || n >= MAX_SEGMENTS {
        *next = 0;
        if n != 0 {
            return None;
        }
    }
    buf[n * PAYLOAD..(n + 1) * PAYLOAD].copy_from_slice(&payload[..PAYLOAD]);
    if header & LAST != 0 {
        *next = 0;
        return Some((n + 1) * PAYLOAD);
    }
    *next = n as u8 + 1;
    None
}

/// One segment of the pad's Bluetooth input. True when it completed a state message: `out` is
/// then the wired frame to forward, header `[0x01, 0x00, 0x01, 0x3C]`.
pub fn classic_ble_input(
    state: &mut [u8; CLASSIC_BLE_STATE],
    segment: &[u8],
    out: &mut [u8; 64],
) -> bool {
    let (s, buf) = state.split_at_mut(S_BUF);
    let Some(len) = assemble(&mut s[S_NEXT], &mut buf[..MESSAGE], segment) else {
        return false;
    };
    let mut msg = [0u8; MESSAGE];
    msg[..len].copy_from_slice(&buf[..len]);
    let msg = &msg[..len];
    if s[S_FLAGS] & STARTED == 0 {
        s[..4].copy_from_slice(&[0x01, 0x00, 0x01, 0x3C]);
        s[S_FLAGS] |= STARTED;
    }
    let folded = match msg {
        [0x01, 0x00, TYPE_BLE_STATE, ..] => fold_type7(s, msg),
        [first, ..] if first & 0x0F == CHUNKED_STATE => fold_chunks(s, msg),
        _ => false, // status and other messages have no wired frame here
    };
    if folded {
        emit(s, out);
    }
    folded
}

fn fold_chunks(s: &mut [u8], msg: &[u8]) -> bool {
    let mask = u16::from(msg[0] & 0xF0) | u16::from(msg.get(1).copied().unwrap_or(0)) << 8;
    let carried: usize = CHUNKS
        .iter()
        .filter(|(bit, ..)| mask & bit != 0)
        .map(|(_, n, _)| n)
        .sum();
    if msg.len() < 2 + carried {
        return false;
    }
    let mut at = 2;
    for (bit, n, to) in CHUNKS {
        if mask & bit != 0 {
            s[to..to + n].copy_from_slice(&msg[at..at + n]);
            at += n;
        }
    }
    true
}

/// The older message multiplexes bytes 16..20 itself; they go where its byte 10 says, and the
/// one IMU block it carries goes where its type says.
fn fold_type7(s: &mut [u8], msg: &[u8]) -> bool {
    let Some(m) = msg.get(..33) else {
        return false;
    };
    let b10 = m[10];
    let on_pad = b10 & LPAD_TOUCH != 0;
    s[S_BUTTONS..S_BUTTONS + 2].copy_from_slice(&m[8..10]);
    // The stick's turn of a pair still has a finger on the pad.
    s[S_BUTTONS + 2] = (b10 & !LPAD_AND_STICK)
        | if b10 & LPAD_AND_STICK != 0 {
            LPAD_TOUCH
        } else {
            0
        };
    s[11..16].copy_from_slice(&m[11..16]);
    let coords = if on_pad { S_LPAD } else { S_STICK };
    s[coords..coords + 4].copy_from_slice(&m[16..20]);
    s[20..24].copy_from_slice(&m[20..24]);
    let (to, n) = match m[24] {
        1 => (40, 8),
        2 => (28, 6),
        3 => (34, 6),
        _ => return true,
    };
    s[to..to + n].copy_from_slice(&m[25..25 + n]);
    true
}

/// The frame for the state as it stands. A finger on the left pad with the stick in use
/// alternates pad and stick frames, as the wired firmware does.
fn emit(s: &mut [u8], out: &mut [u8; 64]) {
    let seq = u32::from_le_bytes([s[4], s[5], s[6], s[7]]).wrapping_add(1);
    s[4..8].copy_from_slice(&seq.to_le_bytes());
    let mut b = [s[S_BUTTONS], s[S_BUTTONS + 1], s[S_BUTTONS + 2]];
    let touched = b[2] & LPAD_TOUCH != 0;
    let stick_live = s[S_STICK..S_STICK + 4] != [0; 4];
    b[2] &= !(LPAD_TOUCH | LPAD_AND_STICK);
    let coords = if touched && stick_live {
        let stick_turn = s[S_FLAGS] & STICK_TURN != 0;
        s[S_FLAGS] ^= STICK_TURN;
        b[2] |= LPAD_AND_STICK | if stick_turn { 0 } else { LPAD_TOUCH };
        if stick_turn {
            S_STICK
        } else {
            S_LPAD
        }
    } else if touched {
        b[2] |= LPAD_TOUCH;
        S_LPAD
    } else {
        S_STICK
    };
    out.copy_from_slice(&s[..64]);
    out[8..11].copy_from_slice(&b);
    out[16..20].copy_from_slice(&s[coords..coords + 4]);
    // The wired frame repeats the triggers as u16s, which SDL rebuilds as `(v << 7) | v`.
    for (i, &t) in [s[11], s[12]].iter().enumerate() {
        let wide = (u16::from(t) << 7) | u16::from(t);
        out[24 + 2 * i..26 + 2 * i].copy_from_slice(&wide.to_le_bytes());
    }
}

/// A wired feature frame, report id 0 first, as the segments the pad takes on Bluetooth. Only the
/// command's own bytes travel: `[command, length, length bytes]`.
pub fn classic_ble_segments(frame: &[u8]) -> impl Iterator<Item = [u8; SEGMENT]> + '_ {
    let body = frame.get(1..).unwrap_or_default();
    let body = body.get(..MESSAGE.min(body.len())).unwrap_or_default();
    let n = body
        .get(1)
        .map_or(body.len(), |&len| (2 + usize::from(len)).min(body.len()));
    let count = n.div_ceil(PAYLOAD);
    body[..n]
        .chunks(PAYLOAD)
        .enumerate()
        .map(move |(i, chunk)| {
            let mut seg = [0u8; SEGMENT];
            seg[0] = REPORT;
            seg[1] = DATA | i as u8 | if i + 1 == count { LAST } else { 0 };
            seg[2..2 + chunk.len()].copy_from_slice(chunk);
            seg
        })
}

/// The segments read back for one feature query, in the order they came. Once the last is in,
/// the reply goes into `out` with report id 0 first, and this returns its length.
pub fn classic_ble_reply(segments: &[u8], out: &mut [u8; 1 + MESSAGE]) -> Option<usize> {
    let mut next = 0;
    let mut buf = [0u8; MESSAGE];
    for seg in segments.chunks(SEGMENT) {
        if let Some(len) = assemble(&mut next, &mut buf, seg) {
            out[0] = 0;
            out[1..1 + len].copy_from_slice(&buf[..len]);
            return Some(1 + len);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `msg` as the pad sends it: zero-padded segments, the last one flagged.
    fn segments(msg: &[u8]) -> Vec<[u8; SEGMENT]> {
        let count = msg.len().div_ceil(PAYLOAD);
        msg.chunks(PAYLOAD)
            .enumerate()
            .map(|(i, c)| {
                let mut s = [0u8; SEGMENT];
                s[0] = REPORT;
                s[1] = DATA | i as u8 | if i + 1 == count { LAST } else { 0 };
                s[2..2 + c.len()].copy_from_slice(c);
                s
            })
            .collect()
    }

    fn feed(state: &mut [u8; CLASSIC_BLE_STATE], msg: &[u8]) -> Option<[u8; 64]> {
        let mut out = [0u8; 64];
        let mut got = None;
        for seg in segments(msg) {
            if classic_ble_input(state, &seg, &mut out) {
                got = Some(out);
            }
        }
        got
    }

    #[test]
    fn a_chunked_state_becomes_a_wired_frame() {
        let mut st = [0u8; CLASSIC_BLE_STATE];
        // Buttons (A), triggers, stick, right pad and gyro: two segments.
        let mut msg = vec![0x04 | 0x10 | 0x20 | 0x80, 0x02 | 0x08];
        msg.extend([0x80, 0x00, 0x10]); // A, right-pad touch
        msg.extend([0xCA, 0x40]); // triggers
        msg.extend([0x10, 0x27, 0xF0, 0xD8]); // stick 10000, -10000
        msg.extend([0x01, 0x02, 0x03, 0x04]); // right pad
        msg.extend([1, 0, 2, 0, 3, 0]); // gyro
        let f = feed(&mut st, &msg).expect("a frame");
        assert_eq!(f[..4], [0x01, 0x00, 0x01, 0x3C]);
        assert_eq!(f[4..8], 1u32.to_le_bytes());
        assert_eq!(f[8..13], [0x80, 0x00, 0x10, 0xCA, 0x40]);
        assert_eq!(f[16..24], [0x10, 0x27, 0xF0, 0xD8, 1, 2, 3, 4]);
        assert_eq!(u16::from_le_bytes([f[24], f[25]]), (0xCA << 7) | 0xCA);
        assert_eq!(f[34..40], [1, 0, 2, 0, 3, 0]);
        // A message that carries only buttons keeps the rest.
        let f = feed(&mut st, &[0x14, 0x00, 0x00, 0x00, 0x00]).expect("a frame");
        assert_eq!(f[4..8], 2u32.to_le_bytes());
        assert_eq!(f[8..11], [0, 0, 0]);
        assert_eq!(f[11..13], [0xCA, 0x40]);
        assert_eq!(f[16..20], [0x10, 0x27, 0xF0, 0xD8]);
    }

    #[test]
    fn a_touched_pad_and_a_live_stick_take_turns() {
        let mut st = [0u8; CLASSIC_BLE_STATE];
        // Finger on the left pad, pad at (1, 1), stick at (2, 2).
        let msg = [0x04 | 0x10 | 0x80, 0x01, 0, 0, 0x08, 2, 0, 2, 0, 1, 0, 1, 0];
        let a = feed(&mut st, &msg).unwrap();
        let b = feed(&mut st, &[0x04, 0x00]).unwrap();
        assert_eq!(
            (a[10], a[16]),
            (LPAD_TOUCH | LPAD_AND_STICK, 1),
            "the pad's turn"
        );
        assert_eq!((b[10], b[16]), (LPAD_AND_STICK, 2), "the stick's turn");
        // The stick back at rest: pad frames only.
        let c = feed(&mut st, &[0x84, 0x00, 0, 0, 0, 0]).unwrap();
        assert_eq!((c[10], c[16]), (LPAD_TOUCH, 1));
        // Finger off: the stick's bytes.
        let d = feed(&mut st, &[0x14, 0x00, 0, 0, 0]).unwrap();
        assert_eq!((d[10], d[16]), (0, 0));
    }

    #[test]
    fn the_older_state_message_folds_too() {
        let mut st = [0u8; CLASSIC_BLE_STATE];
        let mut msg = [0u8; 36];
        msg[..4].copy_from_slice(&[0x01, 0x00, TYPE_BLE_STATE, 32]);
        msg[8] = 0x80;
        msg[16..20].copy_from_slice(&[5, 0, 6, 0]); // the stick
        msg[24] = 3; // gyro
        msg[25..31].copy_from_slice(&[7, 0, 8, 0, 9, 0]);
        let f = feed(&mut st, &msg).unwrap();
        assert_eq!(f[8], 0x80);
        assert_eq!(f[16..20], [5, 0, 6, 0]);
        assert_eq!(f[34..40], [7, 0, 8, 0, 9, 0]);
    }

    #[test]
    fn status_and_broken_sequences_make_no_frame() {
        let mut st = [0u8; CLASSIC_BLE_STATE];
        assert!(
            feed(&mut st, &[0x05, 0x00, 0x64]).is_none(),
            "a status message"
        );
        let segs = segments(&[0x14; 30]);
        let mut out = [0u8; 64];
        // The second segment alone is out of order.
        assert!(!classic_ble_input(&mut st, &segs[1], &mut out));
        // Every chunk promised, one segment sent.
        assert!(feed(&mut st, &[0xF4, 0x1F, 0, 0]).is_none());
        // Recovers on the next whole message.
        assert!(feed(&mut st, &[0x14, 0x00, 0x80, 0, 0]).is_some());
    }

    #[test]
    fn features_go_out_as_segments_and_replies_come_back_whole() {
        let mut lizard = [0u8; 65];
        lizard[1..15].copy_from_slice(&[0x87, 12, 0x31, 2, 0, 7, 7, 0, 8, 7, 0, 24, 0, 0]);
        let segs: Vec<_> = classic_ble_segments(&lizard).collect();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0][..4], [REPORT, DATA | LAST, 0x87, 12]);
        let mut query = [0u8; 65];
        query[1..4].copy_from_slice(&[0xAE, 0x15, 0x01]);
        let segs: Vec<_> = classic_ble_segments(&query).collect();
        assert_eq!(segs.len(), 2, "23 bytes");
        assert_eq!((segs[0][1], segs[1][1]), (DATA, DATA | LAST | 1));

        let mut reply = vec![0xAE, 0x15, 0x01];
        reply.extend(b"FVPF1234567");
        reply.resize(23, 0);
        let mut read: Vec<u8> = Vec::new();
        read.extend([REPORT, 0x00, 0, 0]); // an empty read first
        read.resize(SEGMENT, 0);
        for s in segments(&reply) {
            read.extend(s);
        }
        let mut out = [0u8; 1 + MESSAGE];
        let n = classic_ble_reply(&read, &mut out).expect("a reply");
        assert_eq!(out[..4], [0x00, 0xAE, 0x15, 0x01]);
        assert_eq!(&out[4..15], b"FVPF1234567");
        assert_eq!(n, 1 + 2 * PAYLOAD);
        assert!(
            classic_ble_reply(&read[..SEGMENT * 2], &mut out).is_none(),
            "not yet"
        );
    }
}
