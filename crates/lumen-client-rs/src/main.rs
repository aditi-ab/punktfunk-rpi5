//! `lumen-client-rs` — the reference client (plan M4). Exists to exercise the `lumen/1`
//! (P2) transport: `lumen_core` pulls reassembled, FEC-recovered access units; decode via
//! VAAPI; present via wgpu/Vulkan aligned to client vsync (frame pacing, plan §7).
//!
//! Status: scaffold. The client side of `lumen_core` ([`lumen_core::Session::poll_frame`])
//! is already complete and tested; this binary wires it to a real decoder + presenter.

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!(
        "lumen-client-rs scaffold (lumen_core ABI v{})",
        lumen_core::ABI_VERSION
    );
    tracing::info!(
        "intended flow: lumen_core::Session(client) over UDP → poll_frame → VAAPI decode → wgpu present"
    );
}
