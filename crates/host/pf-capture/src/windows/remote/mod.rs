//! The remote-encoder source: a capture source whose encoder runs in another process, the
//! pf-vdisplay driver's WUDFHost (IDD-push) or a capture worker (WGC). Every module but the
//! four Windows shells is pure, so a whole session runs over the fake driver on every target.
//! Evidence: `design/pf-capture-overhaul.md` §4.4, `design/windows-video-plane-overhaul.md` §2.4.
//!
//! - `source.rs`: [`source::RemoteSource`], one source for both over a [`transport::Transport`].
//! - `transport.rs`: the source's edge into the far process, and the handle-planting seam.
//! - `encoder.rs`: the encoder proxy the loop drives, over the AU section the far side fills.
//! - `au_reader.rs`: the AU section's reader half: the published slots in `wire_seq` order.
//! - `au_assembly.rs`: the wire chunks and whole access units built from those slots.
//! - `delivery.rs`: the gate that turns the far pool's count into a pixel-less delivery.
//! - `supervisor.rs`: the classifier and the recovery ladder over the source's clocks.
//! - `link.rs`: the host's end of a capture worker's two pipes, and the process behind them.
//! - `fake.rs`: the far side the tests play behind a `Transport`, on a virtual clock.
//! - `broker.rs` (Windows): handle duplication into the driver's WUDFHost or a worker.
//! - `section.rs` (Windows): the host's sealed sections, the AU section among them.
//! - `qpc.rs` (Windows): `Instant`, wall time and the system QPC the far side stamps in.
//! - `clock_boost.rs` (Windows): NVIDIA's low-latency clock boost, held while NVENC runs.

// Off Windows only the tests read these modules.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

pub(crate) mod au_assembly;
pub(crate) mod au_reader;
pub(crate) mod delivery;
pub(crate) mod encoder;
#[cfg(test)]
pub(crate) mod fake;
pub(crate) mod link;
pub(crate) mod source;
pub(crate) mod supervisor;
pub(crate) mod transport;

#[cfg(target_os = "windows")]
pub(crate) mod broker;
#[cfg(target_os = "windows")]
pub(crate) mod clock_boost;
#[cfg(target_os = "windows")]
pub(crate) mod qpc;
#[cfg(target_os = "windows")]
pub(crate) mod section;
