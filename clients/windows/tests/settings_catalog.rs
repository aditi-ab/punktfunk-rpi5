//! The settings rows' catalog test, off Windows. There the client is a stub whose unit tests
//! do not build, and a change to `clients/shared/settings-catalog.json` alone does not start
//! the Windows lane. This builds the rows table by itself, so the lane every change runs
//! holds it to the catalog too. On Windows the same test runs inside the client.
#![cfg(not(windows))]

#[path = "../src/app/settings/rows.rs"]
mod rows;
