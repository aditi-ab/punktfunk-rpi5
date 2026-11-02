//! A small pool of GBM-allocated dmabufs, for capture protocols where the *client*
//! supplies the buffers (`ext-image-copy-capture-v1`), unlike PipeWire where the
//! producer allocates, and the [`FreeList`] an encoder's hold returns them to.
//!
//! Only `libgbm` — already linked for the EGL importer — plus a render-node open.
//! No new build dependency: every Mesa/NVIDIA install ships it.

use anyhow::{bail, Context, Result};
use pf_zerocopy::gbm::{GbmBo, GbmDevice, GBM_BO_USE_RENDERING};
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Render node whose `rdev` matches the `dev_t` a compositor advertised.
///
/// The protocol names the device by number, not path. Scanning `/dev/dri` is the only
/// mapping; `drmGetDeviceNameFromFd2` would need libdrm for one lookup.
pub(super) fn render_node_for(dev: u64) -> Result<std::fs::File> {
    let dir = std::fs::read_dir("/dev/dri").context("open /dev/dri")?;
    for e in dir.flatten() {
        let path = e.path();
        let Ok(md) = std::fs::metadata(&path) else {
            continue;
        };
        use std::os::unix::fs::MetadataExt;
        if md.rdev() == dev {
            return std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("open {}", path.display()));
        }
    }
    bail!("no /dev/dri node with device id {dev:#x} — the compositor named a device this host cannot open")
}

/// GBM buffers the compositor writes and the encoder reads. Each keeps the device alive.
pub(super) struct GbmPool {
    pub(super) bos: Vec<GbmBo>,
}

// SAFETY: the gbm handles, and the `Rc<GbmDevice>` every bo shares, are created, read and
// destroyed only on the capture thread that owns the pool. `Send` exists so the whole pool
// moves there once at construction; nothing published carries a gbm pointer or an `Rc`
// clone — a frame gets a `dup` of a bo's fd, a plain owned fd with no thread affinity.
unsafe impl Send for GbmPool {}

impl GbmPool {
    /// Allocate `count` buffers of `width`×`height` in `fourcc`.
    ///
    /// `modifiers` is what the compositor offered, already narrowed to what the consumer
    /// can import. An empty list (or an allocation that refuses every one) falls back to
    /// the implicit modifier, which is what a LINEAR-only path needs.
    pub(super) fn new(
        node: std::fs::File,
        width: u32,
        height: u32,
        fourcc: u32,
        modifiers: &[u64],
        count: usize,
    ) -> Result<GbmPool> {
        let device = Rc::new(
            GbmDevice::open(node).context("open the GBM device on the compositor's render node")?,
        );
        let bos = (0..count)
            .map(|_| alloc(&device, width, height, fourcc, modifiers))
            .collect::<Result<_>>()?;
        Ok(GbmPool { bos })
    }
}

fn alloc(
    device: &Rc<GbmDevice>,
    width: u32,
    height: u32,
    fourcc: u32,
    modifiers: &[u64],
) -> Result<GbmBo> {
    let bo = GbmBo::alloc(
        device,
        width,
        height,
        fourcc,
        modifiers,
        GBM_BO_USE_RENDERING,
    )
    .or_else(|e| {
        if modifiers.is_empty() {
            return Err(e);
        }
        GbmBo::alloc(device, width, height, fourcc, &[], GBM_BO_USE_RENDERING)
    })?;
    // A multi-plane bo cannot travel our single-fd frame contract.
    if bo.planes > 1 {
        bail!(
            "gbm allocated a {}-plane buffer; the frame contract carries one fd",
            bo.planes
        );
    }
    Ok(bo)
}

/// Which buffers the encoder still holds, and the pipe that wakes the loop when one
/// comes back. A returned buffer must re-arm at once or the capture rate halves.
pub(super) struct FreeList {
    pub(super) free: Mutex<Vec<usize>>,
    /// Written by a dropping [`BufHold`] on whatever thread the encoder used.
    pub(super) wake_w: OwnedFd,
}

impl FreeList {
    pub(super) fn take(&self) -> Option<usize> {
        self.free.lock().ok()?.pop()
    }

    pub(super) fn put(&self, idx: usize) {
        if let Ok(mut f) = self.free.lock() {
            f.push(idx);
        }
        // SAFETY: `wake_w` is this struct's live pipe write end; `write` reads one byte
        // from a local and touches no Rust memory beyond it. A full pipe already has a
        // pending wakeup, so a short write is correct to ignore.
        unsafe {
            let b = 1u8;
            libc::write(self.wake_w.as_raw_fd(), std::ptr::addr_of!(b).cast(), 1);
        }
    }
}

/// Returns its buffer to the pool when the last clone drops.
pub(super) struct BufHold {
    pub(super) list: Arc<FreeList>,
    pub(super) idx: usize,
}

impl Drop for BufHold {
    fn drop(&mut self) {
        self.list.put(self.idx);
    }
}
