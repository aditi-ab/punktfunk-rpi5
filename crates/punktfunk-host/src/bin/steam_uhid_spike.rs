//! M0 recognition spike (THROWAWAY) — `design/steam-controller-deck-support.md` go/no-go gate.
//!
//! Opens `/dev/uhid`, creates a virtual `28DE:1205` Steam Deck controller using
//! [`steam_proto::STEAMDECK_RDESC`], services the kernel handshake (the three event types the
//! DualSense backend does NOT: `UHID_SET_REPORT` must be answered or `hid-steam` stalls ~5 s/cmd),
//! answers `steam_get_serial`, heartbeats a neutral Deck report at 125 Hz, and toggles `BTN_A`
//! every 500 ms so a button event is observable.
//!
//! PASS (GO): `dmesg` shows `hid-steam` binding the device; both a gamepad evdev and an IMU evdev
//! (`INPUT_PROP_ACCELEROMETER`) appear; an evdev reader sees `BTN_A` toggle. Run:
//!   `cargo run -p punktfunk-host --bin steam_uhid_spike -- [seconds]`
//!
//! This binary is deleted once M1's `inject/linux/steam_controller.rs` subsumes it.

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
        serial_reply, serialize_deck_state, SteamState, STEAMDECK_PRODUCT, STEAMDECK_RDESC,
        STEAM_REPORT_LEN, STEAM_VENDOR,
    };

    // /dev/uhid event ABI (linux/uhid.h): a u32 `type` then a __packed union (largest member is
    // uhid_create2_req). Field offsets below are union-start (event byte 4) + struct offset.
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

    // --- UHID_CREATE2: identity + report descriptor ---
    let put_cstr = |ev: &mut [u8], off: usize, cap: usize, s: &str| {
        let n = s.len().min(cap - 1);
        ev[off..off + n].copy_from_slice(&s.as_bytes()[..n]);
    };
    let mut ev = vec![0u8; EVENT_SIZE];
    ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
    put_cstr(&mut ev, 4, 128, "Punktfunk Steam Deck (M0 spike)"); // name[128]
    put_cstr(&mut ev, 132, 64, "punktfunk/steam/0"); // phys[64]
    put_cstr(&mut ev, 196, 64, "punktfunk-steam-0"); // uniq[64]
    ev[260..262].copy_from_slice(&(STEAMDECK_RDESC.len() as u16).to_ne_bytes()); // rd_size
    ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes()); // bus
    ev[264..268].copy_from_slice(&STEAM_VENDOR.to_ne_bytes()); // vendor
    ev[268..272].copy_from_slice(&STEAMDECK_PRODUCT.to_ne_bytes()); // product
    ev[272..276].copy_from_slice(&0x0100u32.to_ne_bytes()); // version
    ev[276..280].copy_from_slice(&0u32.to_ne_bytes()); // country
    ev[280..280 + STEAMDECK_RDESC.len()].copy_from_slice(STEAMDECK_RDESC);
    fd.write_all(&ev).context("write UHID_CREATE2")?;
    eprintln!(
        "UHID_CREATE2 -> 28DE:1205 \"Punktfunk Steam Deck (M0 spike)\", {} byte rdesc; running {seconds}s",
        STEAMDECK_RDESC.len()
    );

    let (mut starts, mut opens, mut gets, mut sets, mut outputs) = (0u32, 0u32, 0u32, 0u32, 0u32);
    let mut seq: u32 = 0;
    let mut a_down = false;
    let start = Instant::now();
    let mut last_hb = start;
    let mut last_toggle = start;
    let mut rbuf = vec![0u8; EVENT_SIZE];

    while start.elapsed() < Duration::from_secs(seconds) {
        // Drain all pending kernel events; reply to the handshake (O_NONBLOCK → WouldBlock = empty).
        while let Ok(n) = fd.read(&mut rbuf) {
            if n < 4 {
                break;
            }
            match u32::from_ne_bytes([rbuf[0], rbuf[1], rbuf[2], rbuf[3]]) {
                UHID_START => {
                    starts += 1;
                    eprintln!("  <- UHID_START");
                }
                UHID_OPEN => {
                    opens += 1;
                    eprintln!("  <- UHID_OPEN (a consumer opened the evdev/hidraw)");
                }
                UHID_STOP => eprintln!("  <- UHID_STOP"),
                UHID_CLOSE => eprintln!("  <- UHID_CLOSE"),
                UHID_OUTPUT => {
                    outputs += 1;
                    let sz = u16::from_ne_bytes([rbuf[4100], rbuf[4101]]) as usize;
                    eprintln!(
                        "  <- UHID_OUTPUT ({sz} bytes, head={:02X?})",
                        &rbuf[4..4 + sz.min(8)]
                    );
                }
                UHID_GET_REPORT => {
                    gets += 1;
                    let id = u32::from_ne_bytes([rbuf[4], rbuf[5], rbuf[6], rbuf[7]]);
                    let rnum = rbuf[8];
                    let reply = serial_reply("PUNKTFUNK01");
                    let mut out = vec![0u8; EVENT_SIZE];
                    out[0..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
                    out[4..8].copy_from_slice(&id.to_ne_bytes()); // id
                    out[8..10].copy_from_slice(&0u16.to_ne_bytes()); // err = 0
                    out[10..12].copy_from_slice(&(reply.len() as u16).to_ne_bytes()); // size
                    out[12..12 + reply.len()].copy_from_slice(&reply);
                    fd.write_all(&out).context("write GET_REPORT_REPLY")?;
                    eprintln!("  <- UHID_GET_REPORT (rnum={rnum}) -> replied serial");
                }
                UHID_SET_REPORT => {
                    sets += 1;
                    let id = u32::from_ne_bytes([rbuf[4], rbuf[5], rbuf[6], rbuf[7]]);
                    let rnum = rbuf[8];
                    let cmd = rbuf[12]; // data[0] = the Steam command id
                    let mut out = vec![0u8; EVENT_SIZE];
                    out[0..4].copy_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
                    out[4..8].copy_from_slice(&id.to_ne_bytes()); // id
                    out[8..10].copy_from_slice(&0u16.to_ne_bytes()); // err = 0 (ack)
                    fd.write_all(&out).context("write SET_REPORT_REPLY")?;
                    eprintln!("  <- UHID_SET_REPORT (rnum={rnum}, cmd=0x{cmd:02X}) -> ack err=0");
                }
                other => eprintln!("  <- UHID event type {other}"),
            }
        }

        // Heartbeat the current state at ~125 Hz (a real Deck streams continuously; silence reads
        // as an unplug to the kernel/SDL).
        if last_hb.elapsed() >= Duration::from_millis(8) {
            last_hb = Instant::now();
            seq = seq.wrapping_add(1);
            let mut st = SteamState::neutral();
            if a_down {
                // M0 parse-path probe: mash every button byte so SOME BTN_* fires regardless of the
                // exact (M1-confirmed) per-bit mapping — proves hid-steam parses our state reports.
                st.b8 = 0xFF;
                st.b9 = 0xFF;
                st.b10 = 0xFF;
                st.b13 = 0xFF;
                st.b14 = 0xFF;
            }
            let mut r = [0u8; STEAM_REPORT_LEN];
            serialize_deck_state(&mut r, &st, seq);
            let mut out = vec![0u8; EVENT_SIZE];
            out[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
            out[4..6].copy_from_slice(&(r.len() as u16).to_ne_bytes()); // input2.size
            out[6..6 + r.len()].copy_from_slice(&r); // input2.data
            fd.write_all(&out).context("write UHID_INPUT2")?;
        }

        // Toggle BTN_A every 500 ms so an evdev reader sees a key event.
        if last_toggle.elapsed() >= Duration::from_millis(500) {
            last_toggle = Instant::now();
            a_down = !a_down;
            eprintln!("BTN_A -> {}", if a_down { "DOWN" } else { "UP" });
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    let mut out = vec![0u8; EVENT_SIZE];
    out[0..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
    let _ = fd.write_all(&out);
    eprintln!(
        "UHID_DESTROY. handshake counts: START={starts} OPEN={opens} GET_REPORT={gets} SET_REPORT={sets} OUTPUT={outputs}"
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("steam_uhid_spike: Linux-only (needs /dev/uhid + the hid-steam kernel driver)");
}
