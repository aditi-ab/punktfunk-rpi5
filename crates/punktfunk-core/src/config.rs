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

    #[test]
    fn rejects_block_exceeding_scheme_ceiling() {
        let mut c = Config::p1_defaults(Role::Host); // Gf8, ceiling 255
        c.fec.max_data_per_block = 250;
        c.fec.fec_percent = 15; // 250 + ceil(250*15/100)=288 > 255
        assert!(c.validate().is_err());
    }
}
