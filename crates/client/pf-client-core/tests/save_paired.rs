//! `trust::save_paired` against a scratch config dir (see `common`).
#![cfg(any(target_os = "linux", windows))]

mod common;

use pf_client_core::trust::{save_paired, KnownHosts};
use std::path::Path;

/// Set on the child only: the scratch dir its body works in.
const CHILD_HOME: &str = "PF_SAVE_PAIRED_TEST_HOME";

/// Every pairing ceremony ends in this save: the host is pinned and `paired`, and a record
/// the user already set up keeps what they set on it.
#[test]
fn a_pairing_pins_the_host_and_keeps_what_the_user_set() {
    common::in_scratch_home(
        "a_pairing_pins_the_host_and_keeps_what_the_user_set",
        CHILD_HOME,
        pair_in,
    );
}

/// The body, run in the child with the config dir its environment names.
fn pair_in(_home: &Path) {
    let fp = [0xab_u8; 32];
    let fp_hex = "ab".repeat(32);

    // A first pairing files the host: pinned, paired, and the wake MAC it came with.
    let mac = vec!["aa:bb:cc:dd:ee:ff".to_string()];
    let paired = save_paired("Desk", "10.0.0.5", 9777, fp, &mac);
    assert_eq!((paired.fp, paired.fp_hex.as_str()), (fp, fp_hex.as_str()));
    paired.saved.expect("the store saves");
    let known = KnownHosts::read();
    assert_eq!(known.hosts.len(), 1);
    let host = &known.hosts[0];
    assert_eq!((host.fp_hex.as_str(), host.paired), (fp_hex.as_str(), true));
    assert_eq!(host.mac, mac);

    // Pairing it again, from a card with no MAC, takes none away and resets nothing the user
    // set on the record.
    KnownHosts::update(|known| {
        known.hosts[0].clipboard_sync = true;
        known.hosts[0].preset_id = Some("night".into());
    })
    .expect("the store saves");
    save_paired("Desk", "10.0.0.5", 9777, fp, &[])
        .saved
        .expect("the store saves");
    let known = KnownHosts::read();
    assert_eq!(known.hosts.len(), 1, "a re-pair is an update");
    let host = &known.hosts[0];
    assert!(host.clipboard_sync && host.paired);
    assert_eq!(host.preset_id.as_deref(), Some("night"));
    assert_eq!(host.mac, mac);
}
