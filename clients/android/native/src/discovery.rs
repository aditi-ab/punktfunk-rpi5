//! LAN host discovery over the same `mdns-sd` path as the desktop clients.
//!
//! Kotlin holds the Wi-Fi multicast lock and owns permission UX. Rust owns the service daemon,
//! folds resolve/remove events into a shared map, and returns a newline-delimited snapshot on poll.
//!
//! Start returns an opaque integer key into an `Arc<Discovery>` table. Poll retains the browse while
//! it reads, stop removes the key, and final drop shuts down the daemon and joins its fold thread.
//! This makes stop-vs-poll races safe without JVM callbacks or Rust pointers crossing JNI.

use crate::session::jni_guard;
use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::jlong;
use jni::EnvUnowned;
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

/// DNS-SD service type punktfunk hosts advertise (host side: `punktfunk_host::discovery`).
const SERVICE_TYPE: &str = "_punktfunk._udp.local.";
/// Wire protocol id in the `proto` TXT record; a host advertising anything else is skipped.
const PROTO: &str = "punktfunk/1";
/// Field separator inside one serialized record (ASCII Unit Separator — never in a field value).
const FIELD_SEP: char = '\u{1f}';

/// One resolved host, serialized to Kotlin as `key␟name␟addr␟port␟fp␟pair␟mac␟os␟mgmt`
/// (`␟` = [`FIELD_SEP`]). Records are newline-joined in a poll snapshot; [`Host::encode`] strips
/// the framing bytes from every field so no value can break it. New fields append (the Kotlin
/// parser tolerates both arities), never reorder.
#[derive(Clone, PartialEq)]
struct Host {
    key: String,
    name: String,
    addr: String,
    port: u16,
    fp: String,
    pair: String,
    /// Wake-on-LAN MAC(s) from the mDNS `mac` TXT (comma-separated), for later wake. Empty if absent.
    mac: String,
    /// OS-identity chain from the mDNS `os` TXT (`linux/fedora/bazzite`, ...), for the host
    /// card's OS icon. Empty if absent (older host).
    os: String,
    /// Management-API port from the mDNS `mgmt` TXT — where the game library is served, distinct
    /// from `port` (the native QUIC plane). `0` if absent. Kotlin persists it on the host record so
    /// a host that moved off 47990 keeps its library once mDNS is no longer reachable.
    mgmt: u16,
}

impl Host {
    fn encode(&self) -> String {
        // mDNS instance labels + TXT values are arbitrary UTF-8 from an UNauthenticated source, so
        // strip the field/record separators: a rogue advert that smuggled '\n'/U+001F could otherwise
        // inject or suppress picker rows. (Trust is still gated on connect — this only protects the
        // list's integrity.)
        fn clean(s: &str) -> String {
            s.replace(['\n', '\r', FIELD_SEP], "")
        }
        format!(
            "{}{FIELD_SEP}{}{FIELD_SEP}{}{FIELD_SEP}{}{FIELD_SEP}{}{FIELD_SEP}{}{FIELD_SEP}{}{FIELD_SEP}{}{FIELD_SEP}{}",
            clean(&self.key),
            clean(&self.name),
            clean(&self.addr),
            self.port,
            clean(&self.fp),
            clean(&self.pair),
            clean(&self.mac),
            clean(&self.os),
            self.mgmt,
        )
    }
}

/// One table-owned browse: its daemon, fullname-keyed host map, and event-fold thread.
struct Discovery {
    daemon: ServiceDaemon,
    hosts: Arc<Mutex<HashMap<String, Host>>>,
    thread: Option<JoinHandle<()>>,
}

impl Discovery {
    fn start() -> Option<Discovery> {
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                log::error!("mDNS daemon failed — discovery disabled: {e}");
                return None;
            }
        };
        let rx = match daemon.browse(SERVICE_TYPE) {
            Ok(r) => r,
            Err(e) => {
                log::error!("mDNS browse failed — discovery disabled: {e}");
                let _ = daemon.shutdown();
                return None;
            }
        };
        let hosts: Arc<Mutex<HashMap<String, Host>>> = Arc::new(Mutex::new(HashMap::new()));
        let map = hosts.clone();
        let spawned = std::thread::Builder::new()
            .name("pf-mdns".into())
            .spawn(move || {
                // Exits when the daemon is shut down (the browse channel closes → recv errors).
                while let Ok(event) = rx.recv() {
                    match event {
                        ServiceEvent::ServiceResolved(info) => {
                            if let Some(host) = resolve(&info) {
                                map.lock()
                                    .unwrap()
                                    .insert(info.get_fullname().to_string(), host);
                            }
                        }
                        ServiceEvent::ServiceRemoved(_ty, fullname) => {
                            map.lock().unwrap().remove(&fullname);
                        }
                        _ => {}
                    }
                }
            });
        let thread = match spawned {
            Ok(t) => t,
            Err(e) => {
                // No `Discovery` exists to run `Drop`, so close the daemon on this construction
                // failure before returning.
                log::error!("mDNS fold thread spawn failed: {e}");
                let _ = daemon.shutdown();
                return None;
            }
        };
        log::info!("native mDNS discovery started ({SERVICE_TYPE})");
        Some(Discovery {
            daemon,
            hosts,
            thread: Some(thread),
        })
    }

    /// Current resolved-host set, newline-joined (empty string = none). Sorted for a stable order
    /// across polls; Kotlin re-sorts by display name.
    fn snapshot(&self) -> String {
        let mut records: Vec<String> = self
            .hosts
            .lock()
            .unwrap()
            .values()
            .map(Host::encode)
            .collect();
        records.sort();
        records.join("\n")
    }

    /// Shut down the daemon and join the event-fold thread once.
    fn stop(&mut self) {
        let _ = self.daemon.shutdown(); // closes the browse channel → the fold thread exits
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop();
    }
}

static NEXT_DISCOVERY_HANDLE: AtomicU64 = AtomicU64::new(0x3000_0000_0000_0001);

fn discoveries() -> &'static Mutex<HashMap<jlong, Arc<Discovery>>> {
    static DISCOVERIES: OnceLock<Mutex<HashMap<jlong, Arc<Discovery>>>> = OnceLock::new();
    DISCOVERIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn insert_discovery(discovery: Discovery) -> jlong {
    let discovery = Arc::new(discovery);
    let mut discoveries = crate::session::lock_recover(discoveries());
    loop {
        let handle = NEXT_DISCOVERY_HANDLE.fetch_add(1, Ordering::Relaxed) as jlong;
        if handle != 0 && !discoveries.contains_key(&handle) {
            discoveries.insert(handle, discovery);
            return handle;
        }
    }
}

fn get_discovery(handle: jlong) -> Option<Arc<Discovery>> {
    if handle == 0 {
        return None;
    }
    crate::session::lock_recover(discoveries())
        .get(&handle)
        .cloned()
}

fn remove_discovery(handle: jlong) -> Option<Arc<Discovery>> {
    if handle == 0 {
        return None;
    }
    crate::session::lock_recover(discoveries()).remove(&handle)
}

/// Build a [`Host`] from a resolved mDNS record, or `None` if it isn't a usable punktfunk host
/// (incompatible advertised proto, or no IPv4 address). IPv4 only on purpose: the core dials with
/// `format!("{host}:{port}").parse::<SocketAddr>()`, which can't parse a bare/scoped IPv6 literal
/// (it needs the `[addr%scope]:port` form), so surfacing a v6-only host would present a card that
/// fails on every tap. Dropping it shows the honest "not found" instead.
fn resolve(info: &ResolvedService) -> Option<Host> {
    let val = |k: &str| info.get_property_val_str(k).unwrap_or("").to_string();
    let proto = val("proto");
    if !proto.is_empty() && proto != PROTO {
        return None; // some other DNS-SD service sharing the type — ignore
    }
    // Deterministic pick from the union of per-interface answers (the host OS's responder
    // contributes VPN/overlay addresses; `iter().next()` on the HashSet dialed an arbitrary
    // one) — same policy as the desktop client, shared in `punktfunk_core::discovery`.
    let candidates: Vec<std::net::Ipv4Addr> = info.get_addresses_v4().into_iter().collect();
    let addr = punktfunk_core::discovery::pick_host_addr(&candidates, val("addr").parse().ok())?
        .to_string();
    let id = val("id");
    let fullname = info.get_fullname();
    Some(Host {
        key: if id.is_empty() {
            fullname.to_string()
        } else {
            id
        },
        name: fullname.split('.').next().unwrap_or("?").to_string(),
        addr,
        port: info.get_port(),
        fp: val("fp"),
        pair: val("pair"),
        mac: val("mac"),
        os: val("os"),
        // 0 = the host didn't advertise one (older host); Kotlin then falls back to 47990.
        mgmt: val("mgmt").parse().unwrap_or(0),
    })
}

/// Start `_punktfunk._udp` browsing and return an opaque table key.
/// Kotlin holds its Wi-Fi multicast lock until stop; `0` reports daemon setup failure.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeDiscoveryStart(
    _env: EnvUnowned,
    _this: JObject,
) -> jlong {
    jni_guard(0, || match Discovery::start() {
        Some(discovery) => insert_discovery(discovery),
        None => 0,
    })
}

/// Return the current newline-delimited host snapshot for one retained browse.
/// Missing or concurrently stopped keys produce an empty string.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeDiscoveryPoll<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    // `with_env` subsumes the `jni_guard` this used to carry: it catches panics at the boundary and
    // `LogErrorAndDefault` logs then yields `JString::default()` — the null reference the old
    // `std::ptr::null_mut()` default returned. Kotlin still sees a null String on failure.
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let out = get_discovery(handle)
            .map(|discovery| discovery.snapshot())
            .unwrap_or_default();
        env.new_string(out)
    })
    .resolve::<LogErrorAndDefault>()
}

/// Remove one browse key; final drop shuts down its daemon and joins the fold thread.
/// Zero, stale, duplicate, and concurrent stops are no-ops.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeDiscoveryStop(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || drop(remove_discovery(handle)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_round_trips_all_fields_with_unit_separator() {
        let h = Host {
            key: "host-123".into(),
            name: "home-worker-2".into(),
            addr: "192.168.1.70".into(),
            port: 9777,
            fp: "ab".repeat(32),
            pair: "required".into(),
            mac: "aa:bb:cc:dd:ee:ff".into(),
            os: "linux/fedora/bazzite".into(),
            mgmt: 47991,
        };
        let encoded = h.encode();
        let fields: Vec<&str> = encoded.split(FIELD_SEP).collect();
        assert_eq!(fields.len(), 9);
        assert_eq!(fields[0], "host-123");
        assert_eq!(fields[1], "home-worker-2");
        assert_eq!(fields[2], "192.168.1.70");
        assert_eq!(fields[3], "9777");
        assert_eq!(fields[4], "ab".repeat(32));
        assert_eq!(fields[5], "required");
        assert_eq!(fields[6], "aa:bb:cc:dd:ee:ff");
        assert_eq!(fields[7], "linux/fedora/bazzite");
        // A NON-default port on purpose: the whole point of carrying this field is the host that
        // moved off 47990, so a test pinned to the default would pass against a hardcoded value.
        assert_eq!(fields[8], "47991");
        assert!(
            !encoded.contains('\n'),
            "a record must never contain the record separator"
        );
    }

    #[test]
    fn encode_strips_injected_separators_from_a_hostile_advert() {
        // A rogue advert could carry framing bytes in its instance label / TXT; encode must strip
        // them so the snapshot stays exactly one record of exactly seven fields.
        let h = Host {
            key: "k\u{1f}injected".into(),
            name: "evil\nhost\r".into(),
            addr: "10.0.0.5".into(),
            port: 9777,
            fp: "ab\u{1f}cd".into(),
            pair: "required\n".into(),
            mac: "aa:bb\u{1f}cc".into(),
            os: "linux\u{1f}evil/arch".into(),
            // A numeric field cannot smuggle a separator — it is formatted from a u16, not cleaned.
            mgmt: 47991,
        };
        let encoded = h.encode();
        assert_eq!(encoded.matches(FIELD_SEP).count(), 8, "exactly nine fields");
        assert!(!encoded.contains('\n') && !encoded.contains('\r'));
        let fields: Vec<&str> = encoded.split(FIELD_SEP).collect();
        assert_eq!(fields[0], "kinjected");
        assert_eq!(fields[1], "evilhost");
        assert_eq!(fields[4], "abcd");
        assert_eq!(fields[5], "required");
        assert_eq!(fields[7], "linuxevil/arch");
    }
}
