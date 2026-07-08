//! Client identity, the known-hosts (pinned fingerprint) store, and app settings.
//!
//! The identity shares `~/.config/punktfunk/client-{cert,key}.pem` with `punktfunk-probe`
//! so a box pairs once whichever client it uses.

use anyhow::{anyhow, Context, Result};
use punktfunk_core::client::NativeClient;
use punktfunk_core::quic::endpoint;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub fn config_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME unset")?;
    Ok(PathBuf::from(home).join(".config/punktfunk"))
}

/// This client's persistent identity, generated on first use — presented on every connect
/// so hosts can recognize it once paired.
pub fn load_or_create_identity() -> Result<(String, String)> {
    let dir = config_dir()?;
    let (cp, kp) = (dir.join("client-cert.pem"), dir.join("client-key.pem"));
    if let (Ok(c), Ok(k)) = (std::fs::read_to_string(&cp), std::fs::read_to_string(&kp)) {
        return Ok((c, k));
    }
    let (c, k) = endpoint::generate_identity().map_err(|e| anyhow!("generate identity: {e}"))?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&cp, &c)?;
    std::fs::write(&kp, &k)?;
    tracing::info!(cert = %cp.display(), "generated client identity");
    Ok((c, k))
}

pub fn hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// One trusted host: its pinned certificate fingerprint plus how we got there (TOFU or a
/// PIN ceremony) and where we last reached it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnownHost {
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// SHA-256 of the host certificate, lowercase hex — the pin for every later connect.
    pub fp_hex: String,
    /// True if trust came from the SPAKE2 PIN ceremony (vs. trust-on-first-use).
    pub paired: bool,
    /// Unix seconds of the last successful connect — the hosts page marks the
    /// most-recent card with the accent bar. `default` so pre-existing stores load.
    #[serde(default)]
    pub last_used: Option<u64>,
    /// Wake-on-LAN MAC(s) (`aa:bb:cc:dd:ee:ff`) learned from the host's mDNS `mac` TXT while it
    /// was online, so we can wake it once it sleeps and stops advertising. `default` so
    /// pre-existing stores load; empty until first learned.
    #[serde(default)]
    pub mac: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct KnownHosts {
    pub hosts: Vec<KnownHost>,
}

impl KnownHosts {
    fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("client-known-hosts.json"))
    }

    pub fn load() -> KnownHosts {
        Self::path()
            .and_then(|p| Ok(std::fs::read_to_string(p)?))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::path()?;
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn find_by_fp(&self, fp_hex: &str) -> Option<&KnownHost> {
        self.hosts.iter().find(|h| h.fp_hex == fp_hex)
    }

    pub fn find_by_addr(&self, addr: &str, port: u16) -> Option<&KnownHost> {
        self.hosts.iter().find(|h| h.addr == addr && h.port == port)
    }

    /// Forget the entry with this fingerprint. Returns true if one was removed (the user
    /// will have to pair/trust again to reconnect).
    pub fn remove_by_fp(&mut self, fp_hex: &str) -> bool {
        let before = self.hosts.len();
        self.hosts.retain(|h| h.fp_hex != fp_hex);
        self.hosts.len() != before
    }

    /// Insert or refresh an entry, keyed by fingerprint. `paired` only ever upgrades
    /// (a later TOFU connect must not demote a PIN-paired host).
    pub fn upsert(&mut self, entry: KnownHost) {
        if let Some(h) = self.hosts.iter_mut().find(|h| h.fp_hex == entry.fp_hex) {
            h.name = entry.name;
            h.addr = entry.addr;
            h.port = entry.port;
            h.paired |= entry.paired;
            // A refresh without a timestamp must not erase the stored one.
            if entry.last_used.is_some() {
                h.last_used = entry.last_used;
            }
            // Likewise a trust-decision upsert (which carries no MAC) must not wipe learned MACs.
            if !entry.mac.is_empty() {
                h.mac = entry.mac;
            }
        } else {
            self.hosts.push(entry);
        }
    }
}

/// Load-upsert-save in one step — the pin every trust decision (TOFU accept, PIN
/// ceremony, delegated approval, headless pairing) ends in.
pub fn persist_host(name: &str, addr: &str, port: u16, fp_hex: &str, paired: bool) {
    let mut known = KnownHosts::load();
    known.upsert(KnownHost {
        name: name.to_string(),
        addr: addr.to_string(),
        port,
        fp_hex: fp_hex.to_string(),
        paired,
        last_used: None,
        mac: Vec::new(),
    });
    let _ = known.save();
}

/// Learn/refresh a saved host's Wake-on-LAN MAC(s) from its live advert (called while the host
/// is online, matched by fingerprint or address). No-op — and no disk write — when unchanged, so
/// the hosts page can call it on every discovery tick without churning the store.
pub fn learn_mac(fp_hex: &str, addr: &str, port: u16, mac: &[String]) {
    if mac.is_empty() {
        return;
    }
    let mut known = KnownHosts::load();
    let Some(h) = known
        .hosts
        .iter_mut()
        .find(|h| (!fp_hex.is_empty() && h.fp_hex == fp_hex) || (h.addr == addr && h.port == port))
    else {
        return;
    };
    if h.mac == mac {
        return;
    }
    h.mac = mac.to_vec();
    let _ = known.save();
}

/// Re-key a saved host's address/port after it rediscovered on a new DHCP lease (matched by
/// fingerprint). No-op — and no disk write — when unchanged. Called from the wake-and-wait flow when
/// a woken host reappears on a different IP than the stored one, so this and future connects dial the
/// live address instead of the stale one.
pub fn rekey_addr(fp_hex: &str, addr: &str, port: u16) {
    if fp_hex.is_empty() {
        return;
    }
    let mut known = KnownHosts::load();
    let Some(h) = known.hosts.iter_mut().find(|h| h.fp_hex == fp_hex) else {
        return;
    };
    if h.addr == addr && h.port == port {
        return;
    }
    h.addr = addr.to_string();
    h.port = port;
    let _ = known.save();
}

/// Stamp "now" as this host's last successful connect (drives the hosts page's
/// most-recent accent). No-op when the fingerprint isn't stored.
pub fn touch_last_used(fp_hex: &str) {
    let mut known = KnownHosts::load();
    if let Some(h) = known.hosts.iter_mut().find(|h| h.fp_hex == fp_hex) {
        h.last_used = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();
        let _ = known.save();
    }
}

/// Run the SPAKE2 PIN ceremony against a host. `device_name` is the label the HOST
/// stores this client under (its paired-devices list); the 90 s budget covers a
/// human-typed PIN. Returns the host's now-verified certificate fingerprint to pin.
pub fn pair_with_host(
    addr: &str,
    port: u16,
    identity: &(String, String),
    pin: &str,
    device_name: &str,
) -> std::result::Result<[u8; 32], punktfunk_core::PunktfunkError> {
    NativeClient::pair(
        addr,
        port,
        (&identity.0, &identity.1),
        pin.trim(),
        device_name,
        std::time::Duration::from_secs(90),
    )
}

/// Probe several hosts for reachability in parallel — one thread each, so the wall-clock cost is
/// ~one `timeout`, not the sum. Each element of the returned vec corresponds by index to
/// `targets`. Wraps the single-host [`NativeClient::probe`] (a bounded, trust-agnostic,
/// mDNS-independent QUIC handshake); used by the hosts page's presence pips and the headless
/// `--list-hosts --probe`.
pub fn probe_reachable_many(
    targets: Vec<(String, u16)>,
    timeout: std::time::Duration,
) -> Vec<bool> {
    let handles: Vec<_> = targets
        .into_iter()
        .map(|(addr, port)| {
            std::thread::spawn(move || NativeClient::probe(&addr, port, timeout))
        })
        .collect();
    handles.into_iter().map(|h| h.join().unwrap_or(false)).collect()
}

/// App settings, persisted as JSON. Stringly-typed gamepad/compositor prefs so the file
/// stays readable; parsed with `*Pref::from_name` at connect time.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Stream mode; `0` = the native size/refresh of the monitor the window is on,
    /// resolved at connect time.
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// Requested encoder bitrate (kbps); 0 = host default.
    pub bitrate_kbps: u32,
    pub gamepad: String,
    /// Stable identity (`vid:pid:name`, see `PadInfo::key`) of the physical controller
    /// forwarded as pad 0; empty = automatic (most recently connected). Applied to the
    /// gamepad service at startup so the choice survives restarts.
    pub forward_pad: String,
    /// Which host compositor backend to request (advisory; the host falls back to
    /// auto-detect when unavailable).
    pub compositor: String,
    /// Grab compositor shortcuts (Alt+Tab, Super…) while input is captured.
    pub inhibit_shortcuts: bool,
    /// Stream the default microphone to the host's virtual mic source.
    pub mic_enabled: bool,
    /// Requested audio channel count: 2 (stereo), 6 (5.1) or 8 (7.1). The host clamps to what it
    /// can capture; the resolved count drives the decoder + playback layout.
    pub audio_channels: u8,
    /// Preferred video codec: `"auto"` (host decides), `"hevc"`, `"h264"`, or `"av1"`. A soft
    /// preference — the host honors it when it can emit it, else falls back to the best shared codec.
    #[serde(default = "default_codec")]
    pub codec: String,
    /// Video decoder preference: `"auto"` (Vulkan Video → VAAPI → software),
    /// `"vulkan"`, `"vaapi"`, `"software"`.
    /// The `PUNKTFUNK_DECODER` env var overrides this (see `video::Decoder::new`).
    pub decoder: String,
    /// Show the on-stream statistics overlay (toggle live with Ctrl+Alt+Shift+S).
    pub show_stats: bool,
    /// Enter fullscreen when a stream starts (F11 / the controller chord / the top-edge
    /// header reveal exit it). Gaming-Mode launches (`--fullscreen`) fullscreen regardless.
    pub fullscreen_on_stream: bool,
    /// Experimental: the game-library browser ("Browse library…" on saved cards) —
    /// mirrors the Apple client's "Show game library" toggle, default off.
    pub library_enabled: bool,
}

fn default_codec() -> String {
    "auto".into()
}

impl Settings {
    /// The `codec` setting as a `quic::CODEC_*` preference bit (`0` = auto).
    pub fn preferred_codec(&self) -> u8 {
        match self.codec.as_str() {
            "h264" | "avc" => punktfunk_core::quic::CODEC_H264,
            "hevc" | "h265" => punktfunk_core::quic::CODEC_HEVC,
            "av1" => punktfunk_core::quic::CODEC_AV1,
            _ => 0,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            width: 0,
            height: 0,
            refresh_hz: 0,
            bitrate_kbps: 0,
            gamepad: "auto".into(),
            forward_pad: String::new(),
            compositor: "auto".into(),
            inhibit_shortcuts: true,
            mic_enabled: false,
            audio_channels: 2,
            codec: "auto".into(),
            decoder: "auto".into(),
            show_stats: true,
            fullscreen_on_stream: true,
            library_enabled: false,
        }
    }
}

impl Settings {
    fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("client-gtk-settings.json"))
    }

    pub fn load() -> Settings {
        Self::path()
            .and_then(|p| Ok(std::fs::read_to_string(p)?))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Ok(p) = Self::path() else { return };
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&p, s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pre-`forward_pad` settings file (≤ 0.5.0) loads with the pin on automatic.
    #[test]
    fn settings_forward_pad_defaults_empty() {
        let old = r#"{"width":1280,"height":720,"refresh_hz":60,"bitrate_kbps":0,
            "gamepad":"auto","compositor":"auto","inhibit_shortcuts":true,"mic_enabled":true}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.forward_pad, "");
        let round: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round.forward_pad, "");
    }
}
