//! M0/M1 on-box validator (THROWAWAY) — `design/steam-controller-deck-support.md`.
//!
//! Creates a virtual `28DE:1205` Steam Deck via `/dev/uhid`, enters `gamepad_mode` (pulses the
//! `b9.6` mode-switch bit ~700 ms — `steam_do_deck_input_event` else early-returns under the
//! default `lizard_mode`), then holds a KNOWN test pattern across every field so an evdev reader can
//! confirm [`steam_proto::serialize_deck_state`] is byte-exact against the running kernel. Services
//! the handshake (incl. `UHID_SET_REPORT`, which the DualSense backend omits) and logs any rumble
//! feedback. Run: `cargo run -p punktfunk-host --bin steam_uhid_spike -- [seconds]`.
//!
//! Deleted once M2's `inject/linux/steam_controller.rs` subsumes it.

#[cfg(target_os = "linux")]
#[path = "../inject/proto/steam_proto.rs"]
mod steam_proto;

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};
    use steam_proto::{
        btn, parse_steam_output, serial_reply, serialize_deck_state, SteamState, STEAMDECK_PRODUCT,
        STEAMDECK_RDESC, STEAM_REPORT_LEN, STEAM_VENDOR,
    };

    // /dev/uhid event ABI (linux/uhid.h): u32 `type` then a __packed union (largest = create2_req).
    const EVENT_SIZE: usize = 4 + 4372;
    const UHID_DESTROY: u32 = 1;
    const UHID_START: u32 = 2;
    const UHID_STOP: u32 = 3;
    const UHID_OPEN: u32 = 4;
    const UHID_CLOSE: u32 = 5;
    const UHID_OUTPUT: u32 = 6;
    const UHID_GET_REPORT: u32 = 9;
    const UHID_GET_REPORT_REPLY: u32 = 10;
    const UHID_CREATE2: u32 = 11;
    const UHID_INPUT2: u32 = 12;
    const UHID_SET_REPORT: u32 = 13;
    const UHID_SET_REPORT_REPLY: u32 = 14;
    const BUS_USB: u16 = 0x03;

    // The held test pattern (post mode-switch). Chosen to exercise distinct fields with distinct,
    // recognizable values; expected evdev result is asserted by the companion reader.
    fn test_pattern() -> SteamState {
        let mut st = SteamState::neutral();
        st.buttons = btn::A | btn::X | btn::L4 | btn::R5 | btn::VIEW | btn::RB;
        st.lx = 8000;
        st.ly = 4000;
        st.rx = -3000;
        st.ry = 6000;
        st.lt = 20000;
        st.rt = 10000;
        st.press(btn::RPAD_TOUCH, true);
        st.rpad_x = 5000;
        st.rpad_y = -5000;
        st.accel = [1000, 2000, 3000];
        st.gyro = [100, 200, 300];
        st
    }

    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(25);

    let mut fd = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/uhid")
        .context("open /dev/uhid (are you in the 'input' group?)")?;

    let put_cstr = |ev: &mut [u8], off: usize, cap: usize, s: &str| {
        let n = s.len().min(cap - 1);
        ev[off..off + n].copy_from_slice(&s.as_bytes()[..n]);
    };
    let mut ev = vec![0u8; EVENT_SIZE];
    ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
    put_cstr(&mut ev, 4, 128, "Punktfunk Steam Deck (spike)"); // name[128]
    put_cstr(&mut ev, 132, 64, "punktfunk/steam/0"); // phys[64]
    put_cstr(&mut ev, 196, 64, "punktfunk-steam-0"); // uniq[64]
    ev[260..262].copy_from_slice(&(STEAMDECK_RDESC.len() as u16).to_ne_bytes());
    ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes());
    ev[264..268].copy_from_slice(&STEAM_VENDOR.to_ne_bytes());
    ev[268..272].copy_from_slice(&STEAMDECK_PRODUCT.to_ne_bytes());
    ev[272..276].copy_from_slice(&0x0100u32.to_ne_bytes());
    ev[276..280].copy_from_slice(&0u32.to_ne_bytes());
    ev[280..280 + STEAMDECK_RDESC.len()].copy_from_slice(STEAMDECK_RDESC);
    fd.write_all(&ev).context("write UHID_CREATE2")?;
    eprintln!(
        "UHID_CREATE2 -> 28DE:1205; pulsing mode-switch then holding test pattern ({seconds}s)"
    );

    let (mut sets, mut gets, mut outputs) = (0u32, 0u32, 0u32);
    let mut seq: u32 = 0;
    let start = Instant::now();
    let mut last_hb = start;
    let mut rbuf = vec![0u8; EVENT_SIZE];

    while start.elapsed() < Duration::from_secs(seconds) {
        while let Ok(n) = fd.read(&mut rbuf) {
            if n < 4 {
                break;
            }
            match u32::from_ne_bytes([rbuf[0], rbuf[1], rbuf[2], rbuf[3]]) {
                UHID_START | UHID_STOP | UHID_CLOSE => {}
                UHID_OPEN => eprintln!("  <- UHID_OPEN (consumer opened the evdev/hidraw)"),
                UHID_OUTPUT => {
                    outputs += 1;
                    let sz = u16::from_ne_bytes([rbuf[4100], rbuf[4101]]) as usize;
                    if let Some(rb) = parse_steam_output(&rbuf[4..4 + sz.min(64)]).rumble {
                        eprintln!("  <- rumble (OUTPUT): {rb:?}");
                    }
                }
                UHID_GET_REPORT => {
                    gets += 1;
                    let id = u32::from_ne_bytes([rbuf[4], rbuf[5], rbuf[6], rbuf[7]]);
                    let reply = serial_reply("PUNKTFUNK01");
                    let mut out = vec![0u8; EVENT_SIZE];
                    out[0..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
                    out[4..8].copy_from_slice(&id.to_ne_bytes());
                    out[8..10].copy_from_slice(&0u16.to_ne_bytes()); // err 0
                    out[10..12].copy_from_slice(&(reply.len() as u16).to_ne_bytes());
                    out[12..12 + reply.len()].copy_from_slice(&reply);
                    fd.write_all(&out).context("GET_REPORT_REPLY")?;
                }
                UHID_SET_REPORT => {
                    sets += 1;
                    let id = u32::from_ne_bytes([rbuf[4], rbuf[5], rbuf[6], rbuf[7]]);
                    // data starts at ev[12]: [report-id 0, cmd, …] — surface rumble if present.
                    if let Some(rb) =
                        parse_steam_output(&rbuf[12..12 + 16.min(EVENT_SIZE - 12)]).rumble
                    {
                        eprintln!("  <- rumble (SET_REPORT): {rb:?}");
                    }
                    let mut out = vec![0u8; EVENT_SIZE];
                    out[0..4].copy_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
                    out[4..8].copy_from_slice(&id.to_ne_bytes());
                    out[8..10].copy_from_slice(&0u16.to_ne_bytes()); // err 0
                    fd.write_all(&out).context("SET_REPORT_REPLY")?;
                }
                other => eprintln!("  <- UHID event type {other}"),
            }
        }

        if last_hb.elapsed() >= Duration::from_millis(8) {
            last_hb = Instant::now();
            seq = seq.wrapping_add(1);
            // First ~700 ms: hold the mode-switch bit (b9.6) to toggle gamepad_mode on. After that:
            // the held test pattern (which must NOT contain b9.6, or it would toggle back).
            let st = if start.elapsed() < Duration::from_millis(700) {
                let mut s = SteamState::neutral();
                s.press(btn::STEAM_MENU_RIGHT, true);
                s
            } else {
                test_pattern()
            };
            let mut r = [0u8; STEAM_REPORT_LEN];
            serialize_deck_state(&mut r, &st, seq);
            let mut out = vec![0u8; EVENT_SIZE];
            out[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
            out[4..6].copy_from_slice(&(r.len() as u16).to_ne_bytes());
            out[6..6 + r.len()].copy_from_slice(&r);
            fd.write_all(&out).context("UHID_INPUT2")?;
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    let mut out = vec![0u8; EVENT_SIZE];
    out[0..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
    let _ = fd.write_all(&out);
    eprintln!("UHID_DESTROY. handshake: GET_REPORT={gets} SET_REPORT={sets} OUTPUT={outputs}");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("steam_uhid_spike: Linux-only (needs /dev/uhid + the hid-steam kernel driver)");
}
