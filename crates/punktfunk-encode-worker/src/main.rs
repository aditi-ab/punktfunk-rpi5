//! `punktfunk-encode-worker` — spawned by `punktfunk-host` for the duration of one PyroWave
//! session, never run by hand. It reads its socket from the inherited fd 3 and speaks only to the
//! parent that spawned it: no Wayland, no D-Bus, no network, no plugins.
//!
//! Everything it does lives in [`pf_encode::worker`]; this file exists so the capability has a
//! **file of its own** (see this crate's Cargo.toml for why that is not negotiable).

fn main() -> std::process::ExitCode {
    // Stderr, inherited from the host, so the worker's lines land in the host's journal next to
    // the session that spawned it. `RUST_LOG` is inherited too, so raising the host's level
    // raises the worker's.
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    #[cfg(target_os = "linux")]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match pf_encode::worker::run_from_args(&args) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                // The host reads the dead socket long before it could read this, so the message is
                // for a human running `journalctl` — say enough to place the failure.
                tracing::error!(error = %format!("{e:#}"), "punktfunk-encode-worker exiting");
                std::process::ExitCode::FAILURE
            }
        }
    }
    // Linux-only by construction: the worker exists for `VK_KHR_global_priority` under
    // `CAP_SYS_NICE`, and the Windows host raises its GPU scheduling priority through WDDM
    // instead (`D3DKMTSetProcessSchedulingPriorityClass`). Packaging never installs this
    // elsewhere; the arm exists so a workspace build stays green on every platform.
    #[cfg(not(target_os = "linux"))]
    {
        tracing::error!(
            "punktfunk-encode-worker is a Linux-only helper and has nothing to do here"
        );
        std::process::ExitCode::FAILURE
    }
}
