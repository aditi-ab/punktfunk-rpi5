//! Wire *vocabulary* between the PipeWire capture thread and the isolated zero-copy GPU-import
//! worker process (`punktfunk-host zerocopy-worker`; design:
//! `design/zerocopy-worker-isolation.md`) — the message types and this protocol's version, and
//! nothing else. The transport they ride on ([`super::ipc`]: SEQPACKET framing, `SCM_RIGHTS`,
//! spawn/reap) is shared with the other workers and is deliberately generic over the body type;
//! each worker's vocabulary stays its own and versions independently.
//!
//! Bodies are small serde_json blobs (~200 B/frame); pixels never cross the socket (they move
//! GPU-side via CUDA IPC, see [`super::cuda::ipc_export`]).

use serde::{Deserialize, Serialize};

/// Bumped on any wire change; the worker echoes it in [`Reply::Ready`] and the host refuses a
/// mismatch. Host and worker are the same binary (`/proc/self/exe`), so this only ever trips on
/// exotic deployment mistakes (a stale binary re-exec'd across an upgrade).
pub const PROTO_VERSION: u32 = 1;

/// How a dmabuf should be imported — mirrors the `EglImporter` entry points.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    /// Tiled dmabuf → EGL/GL de-tile blit → BGRx CUDA buffer.
    Tiled,
    /// Tiled dmabuf → EGL/GL NV12 convert → two-plane CUDA buffer (`PUNKTFUNK_NV12`).
    TiledNv12,
    /// LINEAR dmabuf → Vulkan bridge → BGRx CUDA buffer (gamescope's only offer).
    Linear,
    /// Tiled dmabuf → EGL/GL planar-YUV444 convert → ONE stacked 3-plane CUDA buffer (a 4:4:4
    /// session). APPENDED last: the worker can outlive a replaced host binary, so the earlier
    /// variants' wire tags must never shift — an old worker receiving this fails the decode and
    /// the import-fail machinery handles it like any other worker error.
    Tiled444,
    /// LINEAR dmabuf → Vulkan-bridge compute CSC → two-plane NV12 CUDA buffer (latency plan
    /// T2.5b — the gamescope analogue of [`TiledNv12`](Self::TiledNv12)). Appended last, same
    /// wire-tag rule as [`Tiled444`](Self::Tiled444).
    LinearNv12,
}

/// host → worker.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Request {
    /// The EGL-importable DRM modifiers for `fourcc` (startup, before the stream connects —
    /// the host advertises these to PipeWire).
    Modifiers { fourcc: u32 },
    /// Import one frame. `key` identifies the underlying dmabuf across frames (the host uses
    /// the fd's `st_ino` — unique per dma-buf object); the fd itself rides along as
    /// `SCM_RIGHTS` only on first sight of `key` (`has_fd`), and the worker keeps its dup.
    Import {
        key: u64,
        kind: ImportKind,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
        offset: u32,
        stride: u32,
        has_fd: bool,
    },
    /// The frame buffer previously delivered as `id` is no longer in use — recycle it into the
    /// worker's pool. Fire-and-forget (no reply); may be sent from any host thread.
    Release { id: u32 },
    /// The PipeWire stream renegotiated its format: the buffer pool is gone, so drop all cached
    /// per-`key` state (stored fds, Vulkan per-fd imports). Fire-and-forget.
    ClearCache,
}

/// worker → host.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Reply {
    /// Sent once at startup after EGL + CUDA came up.
    Ready {
        version: u32,
    },
    /// Startup failed (no NVIDIA driver, EGL error, …) — the host falls back to the CPU path,
    /// exactly like an in-process `EglImporter::new()` failure.
    InitErr {
        message: String,
    },
    Modifiers {
        modifiers: Vec<u64>,
    },
    /// The imported frame is complete (the GPU copy already synced worker-side) in buffer `id`.
    /// `desc` rides along the first time `id` is ever delivered — the host opens its CUDA IPC
    /// handles once and caches the mapping for every later frame in the same buffer.
    Frame {
        id: u32,
        desc: Option<BufferDesc>,
    },
    /// The worker has no cached fd for the import's `key` (evicted, or the two sides' caches
    /// diverged) — the host forgets its "already sent" note and retries once WITH the fd.
    NeedFd,
    /// This import failed but the worker is alive (e.g. `EGL_BAD_MATCH` on one buffer).
    Err {
        message: String,
    },
}

/// CUDA IPC identity of one pooled device buffer (sent once per buffer, then referenced by id).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct BufferDesc {
    pub width: u32,
    pub height: u32,
    /// `cuIpcGetMemHandle` blob for the (Y or BGRx) plane — exactly 64 bytes.
    pub y_handle: Vec<u8>,
    pub y_pitch: usize,
    /// NV12 only: the interleaved chroma plane's `(handle, pitch)`.
    pub uv: Option<(Vec<u8>, usize)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imp::ipc;
    use std::os::fd::AsFd;

    /// The vocabulary survives the wire in both directions. (The framing itself — fds, EOF,
    /// timeouts, the descriptor cap — is exercised in [`ipc`]; this only pins the message types.)
    #[test]
    fn round_trip_both_directions() {
        let (a, b) = ipc::socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        let req = Request::Import {
            key: 0xdead_beef_u64,
            kind: ImportKind::TiledNv12,
            width: 5120,
            height: 1440,
            fourcc: 0x3432_5258,
            modifier: Some(0x0300_0000_0000_1234),
            offset: 0,
            stride: 5120 * 4,
            has_fd: false,
        };
        ipc::send(a.as_fd(), &req, None).unwrap();
        let (got, fd) = ipc::recv::<Request>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, req);
        assert!(fd.is_none());

        let reply = Reply::Frame {
            id: 7,
            desc: Some(BufferDesc {
                width: 5120,
                height: 1440,
                y_handle: vec![1u8; 64],
                y_pitch: 5632,
                uv: Some((vec![2u8; 64], 5632)),
            }),
        };
        ipc::send(b.as_fd(), &reply, None).unwrap();
        let (got, fd) = ipc::recv::<Reply>(a.as_fd(), &mut buf).unwrap();
        assert_eq!(got, reply);
        assert!(fd.is_none());
    }
}
