//! JNI seam for Wake-on-LAN: parse the stored MAC strings and hand them to the shared core sender
//! (`punktfunk_core::wol`). Like [`crate::discovery`], this takes no session handle — a sleeping
//! host has no ARP entry, so the broadcast the core sends is what wakes it, and Kotlin calls this
//! just before connecting to an offline saved host.

use jni::objects::{JObject, JString};
use jni::JNIEnv;

/// `NativeBridge.nativeWakeOnLan(macsCsv: String, lastIp: String): Boolean` — send a Wake-on-LAN
/// magic packet. `macsCsv` is comma-separated MACs (`aa:bb:..,cc:dd:..`, learned from the host's
/// mDNS `mac` TXT while it was online); `lastIp` is the host's last-known IPv4 (or empty).
/// Returns true if at least one datagram went out.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeWakeOnLan<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    macs_csv: JString<'local>,
    last_ip: JString<'local>,
) -> jni::sys::jboolean {
    let macs_csv: String = match env.get_string(&macs_csv) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let last_ip: String = env
        .get_string(&last_ip)
        .map(|s| Into::<String>::into(s))
        .unwrap_or_default();
    let macs: Vec<[u8; 6]> = macs_csv
        .split(',')
        .filter_map(|s| punktfunk_core::wol::parse_mac(s.trim()))
        .collect();
    if macs.is_empty() {
        return 0;
    }
    let ip = last_ip.trim().parse::<std::net::Ipv4Addr>().ok();
    match punktfunk_core::wol::send_magic_packet(&macs, ip) {
        Ok(()) => 1,
        Err(_) => 0,
    }
}
