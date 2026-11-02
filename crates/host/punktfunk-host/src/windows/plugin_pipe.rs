//! A named-pipe instance with the plugin pipe's DACL. The one unsafe step of `mgmt::pipes`,
//! which is `forbid(unsafe_code)`: the SDDL becomes a descriptor, the instance copies it at
//! create time, and the descriptor is freed before this returns.

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

/// SYSTEM and Administrators in full; LocalService — the runner, and the user half of a
/// container's check — may open, read and write, as may the plugin's own package when it has
/// one. `P`: nothing inherited. Never `ALL APPLICATION PACKAGES`.
fn pipe_sddl(package_sid: Option<&str>) -> String {
    let package = package_sid
        .map(|sid| format!("(A;;FRFW;;;{sid})"))
        .unwrap_or_default();
    format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;LS){package}")
}

/// One instance of `name` under [`pipe_sddl`]. `first` claims the name: a second server on
/// it, from any account, fails instead of sharing the plugin's traffic. Remote clients are
/// refused by the pipe itself.
pub(crate) fn create_plugin_pipe(
    name: &str,
    first: bool,
    package_sid: Option<&str>,
) -> std::io::Result<NamedPipeServer> {
    let sa = pf_paths_win::SecurityAttributes::new(&pipe_sddl(package_sid))
        .map_err(std::io::Error::other)?;
    // SAFETY: `sa` outlives the call, which copies the descriptor into the pipe and writes
    // nothing through the pointer.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, sa.as_ptr().cast_mut().cast())
    }
}

#[cfg(test)]
mod tests {
    /// A package gets its own ACE after the fixed three; none means the three alone.
    #[test]
    fn the_dacl_names_the_package_when_there_is_one() {
        assert_eq!(
            super::pipe_sddl(None),
            "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;LS)"
        );
        assert_eq!(
            super::pipe_sddl(Some("S-1-15-2-1-2-3")),
            "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;LS)(A;;FRFW;;;S-1-15-2-1-2-3)"
        );
    }

    /// A real instance under the descriptor: the first claims the name, and a second claim
    /// on it is refused.
    #[tokio::test]
    async fn the_first_instance_claims_the_name() {
        let name = format!(r"\\.\pipe\pf-plugin-pipe-test-{}", std::process::id());
        let _first = super::create_plugin_pipe(&name, true, None).expect("create the pipe");
        assert!(super::create_plugin_pipe(&name, true, None).is_err());
    }
}
