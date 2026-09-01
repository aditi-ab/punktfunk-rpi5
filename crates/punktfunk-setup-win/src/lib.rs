//! The Windows installer wizard as a library, so the flow tests can drive it through
//! reactor's headless harness (`tests/wizard_flow.rs`) the way `clients/windows` drives its
//! shell. `main.rs` is the thin exe over this.

#[cfg(windows)]
pub mod brand;
#[cfg(windows)]
pub mod silent;
#[cfg(windows)]
pub mod wizard;
