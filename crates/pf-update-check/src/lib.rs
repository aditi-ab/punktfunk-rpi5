//! Shared update-**check** core for the punktfunk host and the Linux client.
//!
//! Both products answer the same question — *does a newer build exist for this box's
//! channel?* — from the same Ed25519-signed per-channel manifest. This crate owns the part
//! where being wrong is a security bug, so that it exists exactly once:
//!
//! * [`sig`] — detached Ed25519 verification against pinned keys.
//! * [`manifest`] — the signed document's schema and its fail-closed validation rules.
//! * [`feed`] — fetching the document (and verifying over the *final*, post-redirect bytes).
//! * [`version`] — channels, and the comparison that decides "newer" across packaging formats.
//! * [`detect`] — the install-kind ladder, parameterised by [`detect::Product`].
//!
//! There is deliberately **no apply code here**. Applying an update is privileged, per-product
//! and per-platform; it lives with the product that does it (`punktfunk-host::update`,
//! `pf-client-core::update`, and the root helper in `pf-update`).

/// The Ed25519 public keys trusted for update manifests — two slots, so a key rotation is
/// "sign with the new one, ship builds trusting both, retire the old" (the plugin-store
/// `OFFICIAL_KEYS` drill) rather than a flag day. The private half is the
/// `UPDATE_MANIFEST_KEY` CI secret and the operator's offline backup.
///
/// It lives here, not in either binary, because the host and the client consume the SAME
/// signed manifest: two pin lists could disagree about who may announce a release, and the
/// one that drifted would be the one nobody noticed. `scripts/ci/publish-update-manifest.sh`
/// cross-checks the signing key against this file before it signs anything.
pub const OFFICIAL_UPDATE_KEYS: [&str; 2] = [
    "ed25519:6rmlLg1aQ55cgB6icpC5BEpbMJxwPKdGaDQtDcJ0yLI=",
    "", // rotation slot
];

pub mod detect;
pub mod feed;
pub mod manifest;
pub mod sig;
pub mod version;

pub use detect::{InstallKind, Product};
pub use feed::FeedError;
pub use manifest::{Manifest, MAX_MANIFEST_BYTES, SCHEMA};
pub use sig::{verify_signature, PublicKey};
pub use version::{canary_run, is_newer, triple, Channel};
