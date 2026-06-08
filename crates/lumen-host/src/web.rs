//! Web config / pairing API (plan §4) — control plane only. This is where `tokio`/`axum`
//! are permitted; the per-frame pipeline never touches them. Serves pairing, client
//! identity/permissions, and surfaces [`lumen_core::Stats`] for the measurement UI (M3).

use anyhow::Result;

/// Control-plane configuration server. Stub until the pairing/RTSP surface is scoped
/// (plan §12 action 4: confirm exactly which serverinfo/RTSP/pairing messages a current
/// Moonlight client needs for P1).
pub struct WebConfig {
    pub bind: String,
}

impl WebConfig {
    pub fn new(bind: impl Into<String>) -> Self {
        WebConfig { bind: bind.into() }
    }

    /// Run the control-plane server. TODO(M2): axum + tokio; GameStream `serverinfo`,
    /// pairing handshake, RTSP SETUP with the `lumen/1` capability flag for negotiation.
    pub fn run(&self) -> Result<()> {
        anyhow::bail!("web/pairing control plane not yet implemented (M2)")
    }
}
