//! Session configuration and protocol/FEC parameters.

use crate::error::{PunktfunkError, Result};
use crate::packet::{CRYPTO_OVERHEAD, HEADER_LEN, MAX_DATAGRAM_BYTES};
use zeroize::Zeroize;

/// Which side of the stream this session drives.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Host = 0,
    Client = 1,
}

/// Negotiated protocol generation. P1 is GameStream-compatible (GF(2⁸)); P2 is the
/// `punktfunk/1` extension (GF(2¹⁶), multi-block framing, optional QUIC control).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolPhase {
    P1GameStream = 1,
    P2Punktfunk = 2,
}

/// Erasure-coding field. Mirrors the on-wire `fec_scheme` tag.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FecScheme {
    /// GF(2⁸) classic RS — Moonlight/GameStream compatible, ≤ 255 shards/block.
    Gf8 = 0,
    /// GF(2¹⁶) Leopard-RS — SIMD, O(n log n), up to 65535 shards/block.
    Gf16 = 1,
}

impl FecScheme {
    pub fn from_u8(v: u8) -> Option<FecScheme> {
        match v {
            0 => Some(FecScheme::Gf8),
            1 => Some(FecScheme::Gf16),
            _ => None,
        }
    }

    /// Hard per-block total-shard ceiling for the field (data + recovery).
    pub fn max_total_shards(self) -> usize {
        match self {
            FecScheme::Gf8 => 255,
            FecScheme::Gf16 => u16::MAX as usize, // wire fields are u16
        }
    }
}

/// A client-sized display mode the host should produce on the virtual output.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// Which compositor backend a client would like the host to drive for its virtual output.
///
/// Sent in [`Hello`](crate::quic::Hello) as a *preference* and echoed back — resolved to the
/// backend actually chosen — in [`Welcome`](crate::quic::Welcome). `Auto` (the default) lets the
/// host decide (auto-detect from the running desktop). A concrete preference is honored only if
/// that backend is available on the host right now; otherwise the host falls back to auto-detect
/// and reports the real choice in `Welcome`. The wire form is a single byte (`0 = Auto`,
/// `1..=4` concrete), appended to `Hello`/`Welcome` — older peers simply omit/ignore it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CompositorPref {
    /// Let the host pick (auto-detect from the running desktop / its configured default).
    #[default]
    Auto,
    /// KWin / KDE Plasma.
    Kwin,
    /// wlroots (Sway / Hyprland).
    Wlroots,
    /// Mutter / GNOME.
    Mutter,
    /// gamescope (spawned nested — available wherever the binary is installed).
    Gamescope,
}

impl CompositorPref {
    /// Wire byte. `0 = Auto`, `1 = Kwin`, `2 = Wlroots`, `3 = Mutter`, `4 = Gamescope`.
    pub fn to_u8(self) -> u8 {
        match self {
            CompositorPref::Auto => 0,
            CompositorPref::Kwin => 1,
            CompositorPref::Wlroots => 2,
            CompositorPref::Mutter => 3,
            CompositorPref::Gamescope => 4,
        }
    }

    /// Inverse of [`to_u8`](Self::to_u8). An unknown byte decodes to `Auto` — forward-compatible:
    /// a future concrete value a peer doesn't recognize degrades to "let the host decide".
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => CompositorPref::Kwin,
            2 => CompositorPref::Wlroots,
            3 => CompositorPref::Mutter,
            4 => CompositorPref::Gamescope,
            _ => CompositorPref::Auto,
        }
    }

    /// Parse a CLI/config name (case-insensitive, with the usual desktop aliases). `None` for an
    /// unrecognized name, so callers can error rather than silently defaulting to `Auto`.
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "detect" | "default" => CompositorPref::Auto,
            "kwin" | "kde" | "plasma" => CompositorPref::Kwin,
            "wlroots" | "sway" | "hyprland" | "wlr" => CompositorPref::Wlroots,
            "mutter" | "gnome" => CompositorPref::Mutter,
            "gamescope" => CompositorPref::Gamescope,
            _ => return None,
        })
    }

    /// Canonical lowercase identifier (`"auto"`, `"kwin"`, `"wlroots"`, `"mutter"`, `"gamescope"`).
    pub fn as_str(self) -> &'static str {
        match self {
            CompositorPref::Auto => "auto",
            CompositorPref::Kwin => "kwin",
            CompositorPref::Wlroots => "wlroots",
            CompositorPref::Mutter => "mutter",
            CompositorPref::Gamescope => "gamescope",
        }
    }
}

/// Which virtual gamepad the host should create for a client's pads.
///
/// Sent in [`Hello`](crate::quic::Hello) as a *preference* and echoed back — resolved to the
/// backend actually chosen — in [`Welcome`](crate::quic::Welcome). `Auto` (the default) lets the
/// host decide (its `PUNKTFUNK_GAMEPAD` env var, else X-Box 360). A concrete preference is
/// honored only if that backend is available on the host (DualSense / DualShock 4 need Linux UHID);
/// otherwise the host falls back and reports the real choice in `Welcome`. The wire form is a single
/// byte (`0 = Auto`, `1 = Xbox360`, `2 = DualSense`, `3 = XboxOne`, `4 = DualShock4`,
/// `5 = SteamController`, `6 = SteamDeck`), appended to `Hello`/`Welcome` — older peers simply
/// omit/ignore it (an unknown byte degrades to `Auto`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GamepadPref {
    /// Let the host pick (its `PUNKTFUNK_GAMEPAD` env var, else X-Box 360).
    #[default]
    Auto,
    /// uinput X-Box 360 pad (the universal default — every game speaks XInput).
    Xbox360,
    /// UHID DualSense (kernel `hid-playstation`) — adaptive triggers, lightbar, touchpad, motion.
    DualSense,
    /// uinput X-Box One / Series pad — the X-Box 360 backend with the One/Series USB identity
    /// (VID/PID/name), so games show One/Series glyphs. XInput-identical otherwise (impulse-trigger
    /// rumble is unreachable through any virtual pad, so there's no game-visible gain over `Xbox360`).
    XboxOne,
    /// UHID DualShock 4 (kernel `hid-playstation`, ≥ 6.2) — lightbar, touchpad, motion, rumble. Like
    /// `DualSense` minus adaptive triggers / player LEDs / mute. Needs Linux UHID on the host.
    DualShock4,
    /// UHID classic Steam Controller (Valve `28DE:1102`, kernel `hid-steam`) — dual trackpads, gyro,
    /// two grip paddles, trackpad-only haptics. Needs Linux UHID. *(Reserved; its backend is not yet
    /// built — currently folds to `Xbox360`; the Deck identity below is the implemented one.)*
    SteamController,
    /// UHID Steam Deck controller (Valve `28DE:1205`, kernel `hid-steam`) — full Deck gamepad incl.
    /// the four back grips (L4/L5/R4/R5), a right trackpad, and the IMU; re-grabbed by Steam Input
    /// with native glyphs when Steam runs on the host. Needs Linux UHID.
    SteamDeck,
}

impl GamepadPref {
    /// Wire byte. `0 = Auto`, `1 = Xbox360`, `2 = DualSense`, `3 = XboxOne`, `4 = DualShock4`,
    /// `5 = SteamController`, `6 = SteamDeck`.
    pub const fn to_u8(self) -> u8 {
        match self {
            GamepadPref::Auto => 0,
            GamepadPref::Xbox360 => 1,
            GamepadPref::DualSense => 2,
            GamepadPref::XboxOne => 3,
            GamepadPref::DualShock4 => 4,
            GamepadPref::SteamController => 5,
            GamepadPref::SteamDeck => 6,
        }
    }

    /// Inverse of [`to_u8`](Self::to_u8). An unknown byte decodes to `Auto` — forward-compatible:
    /// a future concrete value a peer doesn't recognize degrades to "let the host decide".
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => GamepadPref::Xbox360,
            2 => GamepadPref::DualSense,
            3 => GamepadPref::XboxOne,
            4 => GamepadPref::DualShock4,
            5 => GamepadPref::SteamController,
            6 => GamepadPref::SteamDeck,
            _ => GamepadPref::Auto,
        }
    }

    /// Parse a CLI/config name (case-insensitive, with the usual aliases). `None` for an
    /// unrecognized name, so callers can error rather than silently defaulting to `Auto`.
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "default" => GamepadPref::Auto,
            "xbox" | "xbox360" | "x360" | "uinput" => GamepadPref::Xbox360,
            "dualsense" | "ds" | "ps5" => GamepadPref::DualSense,
            "xboxone" | "xbox-one" | "xone" | "xbox1" | "series" | "xboxseries" => {
                GamepadPref::XboxOne
            }
            "dualshock4" | "dualshock" | "ds4" | "ps4" => GamepadPref::DualShock4,
            "steamdeck" | "steam-deck" | "deck" => GamepadPref::SteamDeck,
            "steamcontroller" | "steam-controller" | "steamcon" => GamepadPref::SteamController,
            _ => return None,
        })
    }

    /// Canonical lowercase identifier (`"auto"`, `"xbox360"`, `"dualsense"`, `"xboxone"`,
    /// `"dualshock4"`, `"steamcontroller"`, `"steamdeck"`).
    pub fn as_str(self) -> &'static str {
        match self {
            GamepadPref::Auto => "auto",
            GamepadPref::Xbox360 => "xbox360",
            GamepadPref::DualSense => "dualsense",
            GamepadPref::XboxOne => "xboxone",
            GamepadPref::DualShock4 => "dualshock4",
            GamepadPref::SteamController => "steamcontroller",
            GamepadPref::SteamDeck => "steamdeck",
        }
    }
}

/// Per-block FEC parameters. Recovery count is derived from `fec_percent` exactly as
/// GameStream does: `m = ceil(k * fec_percent / 100)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FecConfig {
    pub scheme: FecScheme,
    /// Recovery overhead as a percentage of data shards (0 disables FEC).
    pub fec_percent: u8,
    /// Maximum data shards per FEC block; larger frames split into multiple blocks.
    /// GF(2⁸) is bounded at 255 total shards, so keep this ≤ ~200 for `Gf8`.
    pub max_data_per_block: u16,
}

impl FecConfig {
    /// Recovery (parity) shard count for a block of `data_shards` shards.
    pub fn recovery_for(&self, data_shards: usize) -> usize {
        if self.fec_percent == 0 || data_shards == 0 {
            return 0;
        }
        // ceil(k * pct / 100)
        (data_shards * self.fec_percent as usize).div_ceil(100)
    }
}

/// Largest shard payload that still fits a datagram once header + crypto overhead are
/// added. Bounds `shard_payload` so packets never exceed [`MAX_DATAGRAM_BYTES`].
pub const fn max_shard_payload() -> usize {
    MAX_DATAGRAM_BYTES - HEADER_LEN - CRYPTO_OVERHEAD
}

/// Largest **even** shard payload whose sealed wire datagram still fits an unfragmented IPv4/UDP
/// packet on a standard 1500-byte MTU: `1500 − 20 (IPv4) − 8 (UDP) − HEADER_LEN − CRYPTO_OVERHEAD`
/// = 1408. Hosts should default `shard_payload` to this: one byte more and the kernel silently
/// splits EVERY video datagram into two IP fragments (a full frame plus a runt) — either fragment
/// lost = the datagram lost, roughly doubling per-datagram loss on Wi-Fi and eating straight into
/// FEC's recovery margin, plus per-pair kernel reassembly and runt airtime at line rate. (Exactly
/// what the previous hardcoded 1452 did: its MTU math forgot the punktfunk header + crypto ride
/// inside the UDP payload and counted the IP+UDP headers as 8 bytes instead of 28.)
pub const fn mtu1500_shard_payload() -> usize {
    let p = 1500 - 20 - 8 - HEADER_LEN - CRYPTO_OVERHEAD;
    p - p % 2 // FEC requires even shards
}

/// The IPv6 sibling of [`mtu1500_shard_payload`]: largest **even** shard payload whose sealed wire
/// datagram fits an unfragmented IPv6/UDP packet on a standard 1500-byte MTU:
/// `1500 − 40 (IPv6) − 8 (UDP) − HEADER_LEN − CRYPTO_OVERHEAD` = 1388. The 20 extra header bytes
/// matter MORE here than on v4: IPv6 routers never fragment — an oversized datagram gets an ICMPv6
/// Packet-Too-Big at best and a silent blackhole at worst — so streaming the v4 size (1408) to a
/// v6 client wouldn't degrade the way v4 fragmentation did (the b5c30df saga), it would drop every
/// video datagram on any 1500-MTU hop.
pub const fn mtu1500_shard_payload_v6() -> usize {
    let p = 1500 - 40 - 8 - HEADER_LEN - CRYPTO_OVERHEAD;
    p - p % 2 // FEC requires even shards
}

/// The MTU-safe shard payload for a session streaming to `peer` (the QUIC remote — the data plane
/// dials the same address family): v6 sizing for a genuine IPv6 remote, v4 sizing otherwise —
/// including IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`, what a dual-stack `[::]` socket reports
/// for a v4 client), which ride IPv4 on the wire. Hosts pass this through
/// `Welcome::shard_payload`, so per-family sizing needs no wire change and old clients simply
/// follow the negotiated value.
pub fn mtu1500_shard_payload_for(peer: core::net::IpAddr) -> usize {
    match peer {
        core::net::IpAddr::V4(_) => mtu1500_shard_payload(),
        core::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some() => mtu1500_shard_payload(),
        core::net::IpAddr::V6(_) => mtu1500_shard_payload_v6(),
    }
}

/// Everything needed to construct a [`Session`](crate::session::Session).
///
/// `Debug` is implemented by hand to redact `key`/`salt`, and `key`/`salt` are zeroized
/// on drop, so secrets neither leak into logs nor linger in freed memory.
#[derive(Clone)]
pub struct Config {
    pub role: Role,
    pub phase: ProtocolPhase,
    pub fec: FecConfig,
    /// Shard payload bytes per packet. Must be even and ≤ [`max_shard_payload`].
    pub shard_payload: usize,
    /// Largest encoded access unit the reassembler will accept (bounds memory against
    /// hostile/​corrupt headers; see [`Session`](crate::session::Session)).
    pub max_frame_bytes: usize,
    pub encrypt: bool,
    /// AES-128 session key established during pairing. MUST be unique per session when
    /// `encrypt` is set (see the nonce-uniqueness contract in [`crate::crypto`]).
    pub key: [u8; 16],
    /// Per-session nonce salt, established alongside `key` during pairing. MUST be
    /// unique per (key, session).
    pub salt: [u8; 4],
    /// Test hook: when non-zero, the loopback transport deterministically drops one of
    /// every `loopback_drop_period` packets it sends. 0 = lossless.
    pub loopback_drop_period: u32,
}

impl Drop for Config {
    fn drop(&mut self) {
        self.key.zeroize();
        self.salt.zeroize();
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("role", &self.role)
            .field("phase", &self.phase)
            .field("fec", &self.fec)
            .field("shard_payload", &self.shard_payload)
            .field("max_frame_bytes", &self.max_frame_bytes)
            .field("encrypt", &self.encrypt)
            .field("key", &"<redacted>")
            .field("salt", &"<redacted>")
            .field("loopback_drop_period", &self.loopback_drop_period)
            .finish()
    }
}

impl Config {
    /// Validate every invariant the hot path and the reassembler rely on. Rejecting here
    /// is what keeps the receive-side parser's allocations bounded.
    pub fn validate(&self) -> Result<()> {
        if self.shard_payload == 0 || self.shard_payload % 2 != 0 {
            return Err(PunktfunkError::InvalidArg(
                "shard_payload must be even and > 0",
            ));
        }
        if self.shard_payload > max_shard_payload() {
            return Err(PunktfunkError::InvalidArg(
                "shard_payload too large to fit a datagram (header + crypto overhead)",
            ));
        }
        if self.fec.max_data_per_block == 0 {
            return Err(PunktfunkError::InvalidArg("max_data_per_block must be > 0"));
        }
        // The per-block total (data + recovery) must fit both the field ceiling and the
        // u16 wire fields.
        let k = self.fec.max_data_per_block as usize;
        let total = k + self.fec.recovery_for(k);
        if total > self.fec.scheme.max_total_shards() {
            return Err(PunktfunkError::InvalidArg(
                "max_data_per_block + recovery exceeds the FEC scheme's shard ceiling",
            ));
        }
        if self.max_frame_bytes == 0 {
            return Err(PunktfunkError::InvalidArg("max_frame_bytes must be > 0"));
        }
        // The frame must not need more FEC blocks than the u16 block-count field allows.
        let total_data = self.max_frame_bytes.div_ceil(self.shard_payload).max(1);
        let max_blocks = total_data.div_ceil(k).max(1);
        if max_blocks > u16::MAX as usize {
            return Err(PunktfunkError::InvalidArg(
                "max_frame_bytes too large for this shard/block configuration (block count overflows u16)",
            ));
        }
        if self.encrypt && self.key == [0u8; 16] {
            return Err(PunktfunkError::InvalidArg(
                "encrypt requires a non-zero session key (see crypto nonce-uniqueness contract)",
            ));
        }
        Ok(())
    }

    /// Sensible P1 defaults: GF(2⁸), 15% FEC, ~1 KiB shards, no encryption, 64 MiB frame
    /// cap. When enabling encryption, replace `key`/`salt` with per-session values from
    /// pairing — the all-zero defaults are rejected by [`validate`](Self::validate).
    pub fn p1_defaults(role: Role) -> Self {
        Config {
            role,
            phase: ProtocolPhase::P1GameStream,
            fec: FecConfig {
                scheme: FecScheme::Gf8,
                fec_percent: 15,
                max_data_per_block: 200,
            },
            shard_payload: 1024,
            max_frame_bytes: 64 * 1024 * 1024,
            encrypt: false,
            key: [0u8; 16],
            salt: [0u8; 4],
            loopback_drop_period: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_encrypt_with_zero_key() {
        let mut c = Config::p1_defaults(Role::Host);
        c.encrypt = true; // key is still all-zero
        assert!(c.validate().is_err());
        c.key = [1u8; 16];
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_oversized_shard_payload() {
        let mut c = Config::p1_defaults(Role::Host);
        c.shard_payload = max_shard_payload() + 2; // still even, but won't fit a datagram
        assert!(c.validate().is_err());
    }

    /// Pin the 1500-MTU wire math: the sealed datagram (header + shard + crypto) at the MTU-safe
    /// shard payload must be ≤ 1472 (1500 − IPv4 20 − UDP 8), and one shard-step (+2) above must
    /// not — the regression that shipped as 1452 and IP-fragmented every video datagram.
    #[test]
    fn mtu1500_shard_payload_never_fragments() {
        let p = mtu1500_shard_payload();
        assert_eq!(p % 2, 0, "FEC requires even shards");
        assert!(p <= max_shard_payload());
        let wire = HEADER_LEN + p + CRYPTO_OVERHEAD;
        assert!(wire <= 1472, "sealed datagram {wire} B would IP-fragment");
        assert!(HEADER_LEN + (p + 2) + CRYPTO_OVERHEAD > 1472, "not maximal");
    }

    /// Pin the IPv6 wire math the same way: the sealed datagram must fit 1452 (1500 − IPv6 40 −
    /// UDP 8 — v6 routers don't fragment, so overshooting blackholes rather than degrades) and one
    /// shard-step above must not.
    #[test]
    fn mtu1500_shard_payload_v6_never_blackholes() {
        let p = mtu1500_shard_payload_v6();
        assert_eq!(p % 2, 0, "FEC requires even shards");
        assert!(p <= max_shard_payload());
        let wire = HEADER_LEN + p + CRYPTO_OVERHEAD;
        assert!(
            wire <= 1452,
            "sealed datagram {wire} B exceeds a 1500-MTU IPv6 hop"
        );
        assert!(HEADER_LEN + (p + 2) + CRYPTO_OVERHEAD > 1452, "not maximal");
    }

    /// Family selection: genuine v6 remotes get the v6 size; v4 — including the IPv4-mapped v6
    /// form a dual-stack `[::]` socket reports for a v4 client — keeps the v4 size.
    #[test]
    fn shard_payload_follows_peer_family() {
        use core::net::IpAddr;
        let v4: IpAddr = "192.168.1.50".parse().unwrap();
        let v6: IpAddr = "fd00::50".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.168.1.50".parse().unwrap();
        assert_eq!(mtu1500_shard_payload_for(v4), mtu1500_shard_payload());
        assert_eq!(mtu1500_shard_payload_for(mapped), mtu1500_shard_payload());
        assert_eq!(mtu1500_shard_payload_for(v6), mtu1500_shard_payload_v6());
    }

    #[test]
    fn rejects_block_exceeding_scheme_ceiling() {
        let mut c = Config::p1_defaults(Role::Host); // Gf8, ceiling 255
        c.fec.max_data_per_block = 250;
        c.fec.fec_percent = 15; // 250 + ceil(250*15/100)=288 > 255
        assert!(c.validate().is_err());
    }

    #[test]
    fn gamepad_pref_steam_roundtrip() {
        use GamepadPref::*;
        // Wire-byte round-trip for the Steam additions; an unknown byte still degrades to Auto.
        for (p, b) in [(SteamController, 5u8), (SteamDeck, 6)] {
            assert_eq!(p.to_u8(), b);
            assert_eq!(GamepadPref::from_u8(b), p);
        }
        assert_eq!(GamepadPref::from_u8(99), Auto);
        // Name parsing + canonical-name round-trip.
        assert_eq!(GamepadPref::from_name("steamdeck"), Some(SteamDeck));
        assert_eq!(GamepadPref::from_name("deck"), Some(SteamDeck));
        assert_eq!(
            GamepadPref::from_name("steamcontroller"),
            Some(SteamController)
        );
        assert_eq!(SteamDeck.as_str(), "steamdeck");
        assert_eq!(
            GamepadPref::from_name(SteamController.as_str()),
            Some(SteamController)
        );
    }
}
