//! The GameStream control stream's codec on bytes a connected LAN peer sends under the
//! session key: the scheme sweep that opens a first packet, the lock that reopens it, and
//! the loss-recovery parsers on what it yields and on raw bytes.
//!
//! punktfunk-host is a binary crate, so nothing links it: the std-and-aes-gcm half of its
//! control module is included by path. A finding is a panic on the control thread.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/host/punktfunk-host/src/gamestream/control_wire.rs"]
#[allow(dead_code)]
mod control_wire;

const KEY: [u8; 16] = [0x42; 16];

fuzz_target!(|data: &[u8]| {
    // The control thread's own header test; everything past it is the codec's.
    if data.len() >= 8 && data[0] == 0x01 && data[1] == 0x00 {
        if let Some((scheme, pt)) = control_wire::decrypt_control(&KEY, data, &None) {
            assert!(
                control_wire::decrypt_control(&KEY, data, &Some(scheme)).is_some(),
                "a locked scheme reopens the packet it locked on"
            );
            let _ = control_wire::ControlMsg::parse(&pt);
        }
    }
    let _ = control_wire::ControlMsg::parse(data);
});
