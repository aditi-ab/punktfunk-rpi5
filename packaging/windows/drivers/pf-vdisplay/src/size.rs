//! `.Size` for `IDD_CX_CLIENT_CONFIG`.
//!
//! The oracle uses a *versioned* size — `IddStructures[INDEX]` when the running framework is OLDER than
//! the (1.10) headers we built against (`IddClientVersionHigherThanFramework != 0`). That machinery
//! (`IddClientVersionHigherThanFramework` / `IddStructureCount` / `IddStructures`) only exists in the
//! iddcx ≥1.4 `IddCxStub`; the WDK on the runner/box links the **1.0** stub (the only `IddCxStub.lib`
//! present), which does NOT export those symbols — referencing them is an LNK2019. We target IddCx 1.10
//! against a current framework (framework ≥ client ⇒ `higher == false`), where `size_of` is exactly what
//! the versioned path returns. So use `size_of` directly. (Revisit the versioned path — with a ≥1.4
//! `IddCxStub` linked — only if pre-1.10 Windows must ever be supported, which the punktfunk Windows
//! host does not target.)

use wdk_sys::iddcx;

/// Correct `.Size` for `IDD_CX_CLIENT_CONFIG` on a framework at least as new as our headers.
#[must_use]
pub fn idd_cx_client_config_size() -> Option<u32> {
    u32::try_from(core::mem::size_of::<iddcx::IDD_CX_CLIENT_CONFIG>()).ok()
}
