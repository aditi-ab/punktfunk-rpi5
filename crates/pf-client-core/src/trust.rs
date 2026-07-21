//! Client identity, the known-hosts (pinned fingerprint) store, and app settings.
//!
//! The identity shares `~/.config/punktfunk/client-{cert,key}.pem` (Linux; on Windows
//! `%APPDATA%\punktfunk`, the WinUI shell's directory) with `punktfunk-probe` so a box
//! pairs once whichever client it uses. On Windows the session binary reads the SAME
//! stores the WinUI shell writes — pairing there makes the session connect silently,
//! mirroring the GTK-shell arrangement on Linux. The WinUI shell re-exports THIS module
//! (`clients/windows/src/trust.rs`), so both processes share one `Settings` shape; the
//! shell stays the settings file's only writer (the session only reads). Pre-unification
//! shell files (≤ 0.8.4: `show_hud`, `engine`) still load — see the migration test below.

use anyhow::{anyhow, Context, Result};
use punktfunk_core::client::NativeClient;
use punktfunk_core::quic::endpoint;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub fn config_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let appdata = std::env::var("APPDATA").context("APPDATA unset")?;
        Ok(PathBuf::from(appdata).join("punktfunk"))
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").context("HOME unset")?;
        Ok(PathBuf::from(home).join(".config/punktfunk"))
    }
}

/// This client's persistent identity, generated on first use — presented on every connect
/// so hosts can recognize it once paired.
pub fn load_or_create_identity() -> Result<(String, String)> {
    let dir = config_dir()?;
    let (cp, kp) = (dir.join("client-cert.pem"), dir.join("client-key.pem"));
    if let (Ok(c), Ok(k)) = (std::fs::read_to_string(&cp), std::fs::read_to_string(&kp)) {
        // An older build wrote the key with a plain `fs::write`, which honors the umask and
        // typically lands 0644 — world-readable. Re-lock an existing store on load so upgrades
        // get fixed, not just fresh installs. Best-effort (a read-only store keeps what it has).
        #[cfg(unix)]
        lock_identity_perms(&dir, &kp);
        return Ok((c, k));
    }
    let (c, k) = endpoint::generate_identity().map_err(|e| anyhow!("generate identity: {e}"))?;
    std::fs::create_dir_all(&dir)?;
    // The private key authorizes this client for full remote control of a paired host, so it must
    // never be world-readable: lock the dir to the owner (0700) and create the key 0600 from the
    // start (`fs::write` alone honors the umask → typically 0644). The certificate is public. On
    // non-Unix the %APPDATA% profile ACL already scopes the dir to the user, so std perms suffice.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::write(&cp, &c)?;
    write_private_key(&kp, k.as_bytes())?;
    tracing::info!(cert = %cp.display(), "generated client identity");
    Ok((c, k))
}

/// Write the client's mTLS private key owner-only. On Unix the file is created with mode 0600 from
/// the outset — an `fs::write` + later `chmod` would briefly expose it at the umask default. On
/// other platforms std's default perms plus the %APPDATA% profile ACL scope it to the user.
fn write_private_key(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Best-effort re-lock of an already-present identity (dir 0700, key 0600) — for stores written by
/// an older build that left the key world-readable. Errors are ignored: the worst case is the
/// pre-existing perms, which this never loosens.
#[cfg(unix)]
fn lock_identity_perms(dir: &std::path::Path, key: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let _ = std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o600));
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
    /// Share this machine's clipboard with THIS host (design/clipboard-and-file-transfer.md
    /// §5.3 — the Apple client's `StoredHost.clipboardSync`). Per-host, not global: handing a
    /// host your clipboard is a trust decision about that host. Default off; the host must
    /// also advertise `HOST_CAP_CLIPBOARD` and have its own policy enabled.
    #[serde(default)]
    pub clipboard_sync: bool,
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
        clipboard_sync: false,
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

/// User-facing sentence for a failed connect / request-access, keyed on the actual cause —
/// shared by every desktop/console surface so "the host declined this device" never renders
/// as "connection timed out". Reason-specific text for a typed host rejection
/// ([`punktfunk_core::reject::RejectReason`]); the caller keeps its own wording for
/// non-rejection errors.
pub fn connect_reject_message(reason: punktfunk_core::reject::RejectReason) -> String {
    use punktfunk_core::reject::RejectReason as R;
    match reason {
        R::Denied => "The host declined this device's request.".into(),
        R::ApprovalTimeout => {
            "Nobody approved the request on the host in time — approve this device in the \
             host's console or web UI, then request access again."
                .into()
        }
        R::Superseded => {
            "A newer request from this device replaced this one — approve the latest request \
             on the host."
                .into()
        }
        R::IdentityRequired => {
            "The host requires pairing — pair this device (PIN or request access) first.".into()
        }
        R::PairingNotArmed => {
            "Pairing isn't armed on the host — arm it on the host's Pairing page, then try \
             again."
                .into()
        }
        R::PairingBoundToOtherDevice => {
            "The host's pairing window is armed for a different device — arm it for this one."
                .into()
        }
        R::PairingRateLimited => {
            "Too many pairing attempts — wait a couple of seconds and try again.".into()
        }
        R::WireVersionMismatch => {
            "Client and host versions don't match — update both to the same release.".into()
        }
        R::Busy => "The host is busy with another session.".into(),
        R::SetupFailed => {
            "The host accepted the connection but couldn't start the stream — the host's log \
             (web console → Log) has the cause."
                .into()
        }
    }
}

/// User-facing sentence for a failed PIN pairing ceremony ([`pair_with_host`]) — distinguishes
/// a wrong PIN (the SPAKE2 proof failed) from an unreachable host and from the host's typed
/// rejections, so a dead network path or a disarmed host is never reported as a bad PIN.
pub fn pair_error_message(err: &punktfunk_core::PunktfunkError) -> String {
    use punktfunk_core::PunktfunkError as E;
    match err {
        E::Crypto => "Wrong PIN — check the PIN on the host's Pairing page and try again.".into(),
        E::Rejected(reason) => connect_reject_message(*reason),
        E::Timeout => "The host didn't answer. Is it running and reachable?".into(),
        E::Io(_) => {
            "Couldn't reach the host — check that this device and the host are on the same \
             network (no VPN on this device, no guest-Wi-Fi / AP isolation)."
                .into()
        }
        other => format!("Pairing failed: {other:?}"),
    }
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
        .map(|(addr, port)| std::thread::spawn(move || NativeClient::probe(&addr, port, timeout)))
        .collect();
    handles
        .into_iter()
        .map(|h| h.join().unwrap_or(false))
        .collect()
}

/// How much the on-stream statistics overlay shows — the Android client's tiers, shared
/// across every client (design/stats-unification.md): each tier is a strict superset of
/// the previous. Ctrl+Alt+Shift+S cycles Off → Compact → Normal → Detailed live.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatsVerbosity {
    Off,
    /// One glanceable line: fps · end-to-end ms · Mb/s.
    Compact,
    /// Stream mode plus the end-to-end latency percentiles and loss counters.
    Normal,
    /// Everything: decoder path, HDR tags, and the per-stage latency equation.
    Detailed,
}

impl StatsVerbosity {
    /// Cycle order (also the settings pickers' option order).
    pub const ALL: [StatsVerbosity; 4] = [
        StatsVerbosity::Off,
        StatsVerbosity::Compact,
        StatsVerbosity::Normal,
        StatsVerbosity::Detailed,
    ];

    /// The next tier in the live cycle, wrapping back to Off.
    pub fn next(self) -> StatsVerbosity {
        match self {
            StatsVerbosity::Off => StatsVerbosity::Compact,
            StatsVerbosity::Compact => StatsVerbosity::Normal,
            StatsVerbosity::Normal => StatsVerbosity::Detailed,
            StatsVerbosity::Detailed => StatsVerbosity::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StatsVerbosity::Off => "Off",
            StatsVerbosity::Compact => "Compact",
            StatsVerbosity::Normal => "Normal",
            StatsVerbosity::Detailed => "Detailed",
        }
    }
}

/// How a touchscreen's fingers drive the host — the cross-client touch-input model (Android
/// `TouchMode`, Apple `TouchInputMode`). Stored stringly in [`Settings::touch_mode`] so the
/// file stays readable; parsed with [`TouchMode::from_name`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TouchMode {
    /// Relative cursor like a laptop touchpad: the cursor stays put on touch-down and moves
    /// by the finger's delta (with mild acceleration), tap to click. The default — a cursor
    /// is the universally workable model on a screen the host isn't sized for.
    Trackpad,
    /// Direct pointing: the cursor jumps to the finger and follows it (absolute).
    Pointer,
    /// Real multi-touch passthrough: every finger is a host touchscreen contact, no gesture
    /// interpretation — only helps hosts/apps that actually understand touch.
    Touch,
}

impl TouchMode {
    /// Cycle/picker order (also the settings pickers' option order).
    pub const ALL: [TouchMode; 3] = [TouchMode::Trackpad, TouchMode::Pointer, TouchMode::Touch];

    /// Parse the persisted name, defaulting to `Trackpad` for unset/unknown values.
    pub fn from_name(s: &str) -> TouchMode {
        match s {
            "pointer" => TouchMode::Pointer,
            "touch" => TouchMode::Touch,
            _ => TouchMode::Trackpad,
        }
    }

    /// The persisted name (the inverse of [`from_name`](Self::from_name)).
    pub fn as_name(self) -> &'static str {
        match self {
            TouchMode::Trackpad => "trackpad",
            TouchMode::Pointer => "pointer",
            TouchMode::Touch => "touch",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TouchMode::Trackpad => "Trackpad",
            TouchMode::Pointer => "Direct pointer",
            TouchMode::Touch => "Touch passthrough",
        }
    }
}

/// How a physical mouse drives the host — the desktop-sweep mouse model
/// (design/remote-desktop-sweep.md M1). Stored stringly in [`Settings::mouse_mode`] so the
/// file stays readable; parsed with [`MouseMode::from_name`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseMode {
    /// Pointer lock (relative deltas, hidden cursor) — the game model, and the default:
    /// the only cursor you see is the host's.
    Capture,
    /// Absolute pointer, uncaptured: the cursor enters and leaves the stream freely and
    /// motion goes on the wire as absolute positions through the letterbox. The remote
    /// desktop model. Requires a host injector with absolute support (not gamescope).
    Desktop,
}

impl MouseMode {
    /// Cycle/picker order (also the settings pickers' option order).
    pub const ALL: [MouseMode; 2] = [MouseMode::Capture, MouseMode::Desktop];

    /// Parse the persisted name, defaulting to `Capture` for unset/unknown values.
    pub fn from_name(s: &str) -> MouseMode {
        match s {
            "desktop" => MouseMode::Desktop,
            _ => MouseMode::Capture,
        }
    }

    /// The persisted name (the inverse of [`from_name`](Self::from_name)).
    pub fn as_name(self) -> &'static str {
        match self {
            MouseMode::Capture => "capture",
            MouseMode::Desktop => "desktop",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MouseMode::Capture => "Capture (games)",
            MouseMode::Desktop => "Desktop (absolute)",
        }
    }
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
    /// Render-resolution multiplier: the client asks the host to render/encode at
    /// `resolved mode × render_scale` and the presenter downscales the larger decoded frame to the
    /// window (`> 1` supersamples for sharpness, at more bandwidth AND decode; `< 1` renders under
    /// native for a lighter host/link). `1.0` = Native (the prior behaviour). Applied at connect
    /// (and each match-window resize) via [`punktfunk_core::render_scale`], clamped even + to the
    /// codec's max dimension. Missing in a pre-existing store → the `Default` (1.0) via the
    /// container `#[serde(default)]`.
    pub render_scale: f64,
    pub gamepad: String,
    /// Stable identity (`vid:pid:name`, see `PadInfo::key`) of the physical controller
    /// forwarded as pad 0; empty = automatic (most recently connected). Applied to the
    /// gamepad service at startup so the choice survives restarts.
    pub forward_pad: String,
    /// Which host compositor backend to request (advisory; the host falls back to
    /// auto-detect when unavailable).
    pub compositor: String,
    /// How a touchscreen's fingers drive the host (Deck/tablet): a [`TouchMode`] name —
    /// `"trackpad"` (default), `"pointer"`, or `"touch"`. Read at connect via
    /// [`Settings::touch_mode`]; irrelevant on a mouse-only client. `default` so pre-existing
    /// stores load as trackpad.
    #[serde(default = "default_touch_mode")]
    pub touch_mode: String,
    /// How a physical mouse drives the host: a [`MouseMode`] name — `"capture"` (default,
    /// pointer lock + relative) or `"desktop"` (uncaptured absolute pointer). Read at
    /// connect via [`Settings::mouse_mode`]. `default` so pre-existing stores load as
    /// capture — today's behavior.
    #[serde(default = "default_mouse_mode")]
    pub mouse_mode: String,
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
    /// Decode/present GPU (multi-GPU boxes): the adapter's marketing name, as the WinUI
    /// shell's GPU picker stores it; empty = automatic. The session maps it onto the
    /// presenter's device pick (`PUNKTFUNK_VK_ADAPTER`). `default` so pre-existing
    /// stores (and the Linux shells, which have no picker yet) load.
    #[serde(default)]
    pub adapter: String,
    /// Advertise 10-bit + HDR10 so the host upgrades HDR content to a Main10/PQ stream.
    /// The presenter handles the display side dynamically either way (HDR10 swapchain
    /// where offered, tonemap where not) — off means "never send me 10-bit".
    /// `default = true`: the Linux stores never carried this and always advertised.
    #[serde(default = "default_true")]
    pub hdr_enabled: bool,
    /// Legacy on/off for the stats overlay — superseded by `stats_verbosity` but kept
    /// written in sync (`set_stats_verbosity`) so pre-tier binaries reading the same
    /// file keep working. `alias`: the pre-unification WinUI shell (≤ 0.8.4) persisted
    /// this as `show_hud`.
    #[serde(alias = "show_hud")]
    pub show_stats: bool,
    /// Stats overlay tier. `None` = a pre-tier store; resolve through
    /// [`Settings::stats_verbosity`], which falls back to `show_stats`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats_verbosity: Option<StatsVerbosity>,
    /// Enter fullscreen when a stream starts (F11 / the controller chord / the top-edge
    /// header reveal exit it). Gaming-Mode launches (`--fullscreen`) fullscreen regardless.
    pub fullscreen_on_stream: bool,
    /// Experimental: the game-library browser ("Browse library…" on saved cards) —
    /// mirrors the Apple client's "Show game library" toggle, default off.
    pub library_enabled: bool,
    /// Send Wake-on-LAN before connecting to a saved host and wait for it to boot (the
    /// Apple client's "Auto-wake on connect"). Default ON — that was the unconditional
    /// behavior before this became a setting. Off is for hosts reached over a VPN, where
    /// an offline-looking host is really just unreachable by broadcast and the wake +
    /// wait only adds a delay.
    #[serde(default = "default_true")]
    pub auto_wake: bool,
    /// Reverse the wheel/trackpad scroll direction sent to the host (the Apple client's
    /// "Invert scroll direction"). Default off = the host scrolls the way this machine does.
    #[serde(default)]
    pub invert_scroll: bool,
    /// Playback endpoint for stream audio — on Linux the PipeWire `node.name` the
    /// playback stream targets (`target.object`); empty = the session default (the
    /// Apple client's Speaker picker). The session maps it onto `PUNKTFUNK_AUDIO_SINK`.
    /// Ignored on Windows until the WASAPI endpoint leg exists.
    #[serde(default)]
    pub speaker_device: String,
    /// Capture endpoint for the mic uplink (same semantics as `speaker_device`;
    /// `PUNKTFUNK_AUDIO_SOURCE`).
    #[serde(default)]
    pub mic_device: String,
    /// Match-window resolution policy (design/midstream-resolution-resize.md D1): the
    /// stream mode follows the session window — the connect asks for the window's pixel
    /// size and a mid-session resize renegotiates the host's virtual display + encoder
    /// (`Reconfigure`), so windowed sessions stream native-resolution pixels instead of
    /// scaling. Overrides `width`/`height` while on; on fullscreen it degenerates to the
    /// display's native mode. Default off (Auto-native stays the shipped default until
    /// the per-backend validation matrix is green).
    pub match_window: bool,
    /// The session window's last logical size under `match_window`: the next launch
    /// opens its window at this size, so the first connect's mode already matches what
    /// the user will be looking at. `0` = never stored → the 1280×720 default.
    pub last_window_w: u32,
    pub last_window_h: u32,
}

fn default_codec() -> String {
    "auto".into()
}

fn default_touch_mode() -> String {
    "trackpad".into()
}

fn default_mouse_mode() -> String {
    "capture".into()
}

fn default_true() -> bool {
    true
}

impl Settings {
    /// The stats-overlay tier, resolving pre-tier stores: an old `show_stats = false`
    /// reads as Off, everything else as Normal (≈ what the pre-tier overlay showed).
    pub fn stats_verbosity(&self) -> StatsVerbosity {
        self.stats_verbosity.unwrap_or(if self.show_stats {
            StatsVerbosity::Normal
        } else {
            StatsVerbosity::Off
        })
    }

    /// Set the tier, keeping the legacy `show_stats` bool coherent for pre-tier
    /// binaries that read the same settings file.
    pub fn set_stats_verbosity(&mut self, v: StatsVerbosity) {
        self.stats_verbosity = Some(v);
        self.show_stats = v != StatsVerbosity::Off;
    }

    /// The touch-input model for this session (parsed from the stored name).
    pub fn touch_mode(&self) -> TouchMode {
        TouchMode::from_name(&self.touch_mode)
    }

    pub fn mouse_mode(&self) -> MouseMode {
        MouseMode::from_name(&self.mouse_mode)
    }

    /// The `codec` setting as a `quic::CODEC_*` preference bit (`0` = auto).
    pub fn preferred_codec(&self) -> u8 {
        match self.codec.as_str() {
            "h264" | "avc" => punktfunk_core::quic::CODEC_H264,
            "hevc" | "h265" => punktfunk_core::quic::CODEC_HEVC,
            "av1" => punktfunk_core::quic::CODEC_AV1,
            // The wired-LAN wavelet codec: preference-only by design (resolve_codec never
            // auto-picks it), and harmless on a build/device that doesn't advertise the
            // bit — the ladder falls back to HEVC.
            "pyrowave" => punktfunk_core::quic::CODEC_PYROWAVE,
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
            render_scale: 1.0,
            gamepad: "auto".into(),
            forward_pad: String::new(),
            compositor: "auto".into(),
            touch_mode: "trackpad".into(),
            mouse_mode: "capture".into(),
            inhibit_shortcuts: true,
            mic_enabled: false,
            audio_channels: 2,
            codec: "auto".into(),
            decoder: "auto".into(),
            adapter: String::new(),
            hdr_enabled: true,
            show_stats: true,
            stats_verbosity: None,
            fullscreen_on_stream: true,
            library_enabled: false,
            auto_wake: true,
            invert_scroll: false,
            speaker_device: String::new(),
            mic_device: String::new(),
            match_window: false,
            last_window_w: 0,
            last_window_h: 0,
        }
    }
}

impl Settings {
    fn path() -> Result<PathBuf> {
        // The shell's settings file on each OS: the GTK shell's on Linux, the WinUI
        // shell's on Windows. The desktop shells AND the session binary's console
        // settings screen write it (load-modify-save per change — Gaming Mode has no
        // other editor); a plain `--connect` stream only ever reads.
        #[cfg(windows)]
        return Ok(config_dir()?.join("client-windows-settings.json"));
        #[cfg(not(windows))]
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

    /// A settings file predating the touch-input model loads as `trackpad` (the shipped
    /// default), and the name round-trips through the enum both ways.
    #[test]
    fn settings_touch_mode_defaults_trackpad() {
        let old = r#"{"width":1280,"height":720,"gamepad":"auto","compositor":"auto"}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.touch_mode, "trackpad");
        assert_eq!(s.touch_mode(), TouchMode::Trackpad);
        // Explicit values parse; an unknown name falls back to trackpad.
        assert_eq!(TouchMode::from_name("pointer"), TouchMode::Pointer);
        assert_eq!(TouchMode::from_name("touch"), TouchMode::Touch);
        assert_eq!(TouchMode::from_name("bogus"), TouchMode::Trackpad);
        for m in TouchMode::ALL {
            assert_eq!(TouchMode::from_name(m.as_name()), m);
        }
    }

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

    /// A pre-unification WinUI shell settings file (≤ 0.8.4, when the shell had its own
    /// `Settings` struct) still loads: `show_hud` migrates onto `show_stats` via the serde
    /// alias, the dropped `engine` knob is ignored, fields that file never carried
    /// (forward_pad, fullscreen_on_stream, …) default, and the D3D11VA-era
    /// `decoder: "hardware"` survives as-is (video::Decoder::new reads it as auto).
    #[test]
    fn settings_reads_winui_shell_shape() {
        let shell = r#"{
            "width": 2560, "height": 1440, "refresh_hz": 120, "bitrate_kbps": 20000,
            "gamepad": "dualsense", "compositor": "auto",
            "inhibit_shortcuts": true, "mic_enabled": true, "audio_channels": 6,
            "hdr_enabled": true, "decoder": "hardware", "codec": "av1",
            "adapter": "NVIDIA GeForce RTX 4080", "show_hud": false, "engine": "builtin"
        }"#;
        let s: Settings = serde_json::from_str(shell).unwrap();
        assert_eq!((s.width, s.height, s.refresh_hz), (2560, 1440, 120));
        assert_eq!(s.bitrate_kbps, 20000);
        assert_eq!(s.audio_channels, 6);
        assert!(s.mic_enabled);
        assert_eq!(s.decoder, "hardware");
        assert_eq!(s.preferred_codec(), punktfunk_core::quic::CODEC_AV1);
        let mut pw = s.clone();
        pw.codec = "pyrowave".into();
        assert_eq!(pw.preferred_codec(), punktfunk_core::quic::CODEC_PYROWAVE);
        assert_eq!(s.adapter, "NVIDIA GeForce RTX 4080");
        assert!(s.hdr_enabled);
        // The old shell's `show_hud` lands on `show_stats` (the user's preference survives).
        assert!(!s.show_stats);
        // Fields the old file doesn't carry take this struct's defaults.
        assert_eq!(s.forward_pad, "");
        assert!(s.fullscreen_on_stream);
        assert!(!s.library_enabled);
    }

    /// Stats-tier resolution: a pre-tier store falls back to `show_stats` (off → Off,
    /// on/absent → Normal), an explicit tier wins, and setting a tier keeps the legacy
    /// bool in sync so pre-tier binaries reading the same file agree on off vs on.
    #[test]
    fn stats_verbosity_migrates_and_round_trips() {
        let mut s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.stats_verbosity(), StatsVerbosity::Normal);
        let off: Settings = serde_json::from_str(r#"{"show_stats":false}"#).unwrap();
        assert_eq!(off.stats_verbosity(), StatsVerbosity::Off);

        s.set_stats_verbosity(StatsVerbosity::Compact);
        assert!(s.show_stats);
        s.set_stats_verbosity(StatsVerbosity::Off);
        assert!(!s.show_stats);

        s.set_stats_verbosity(StatsVerbosity::Detailed);
        let round: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round.stats_verbosity(), StatsVerbosity::Detailed);
        // The tier serializes lowercase — the file stays human-readable.
        assert!(serde_json::to_string(&s).unwrap().contains("\"detailed\""));
    }

    /// The WinUI shell's known-hosts shape (no `last_used` field) loads losslessly — same
    /// filename, same directory, so on Windows the two clients genuinely share the store.
    #[test]
    fn known_hosts_reads_winui_shell_shape() {
        let shell = r#"{"hosts":[{
            "name": "Gaming PC", "addr": "192.168.1.50", "port": 9777,
            "fp_hex": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "paired": true, "mac": ["aa:bb:cc:dd:ee:ff"]
        }]}"#;
        let k: KnownHosts = serde_json::from_str(shell).unwrap();
        let h = k.find_by_addr("192.168.1.50", 9777).unwrap();
        assert!(h.paired);
        assert_eq!(h.last_used, None);
        assert_eq!(h.mac, vec!["aa:bb:cc:dd:ee:ff".to_string()]);
        assert!(parse_hex32(&h.fp_hex).is_some());
    }
}
