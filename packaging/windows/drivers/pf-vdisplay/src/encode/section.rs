//! The host-created AU section, mapped and written from the driver side, and the
//! [`EncodeSession`] one `SET_ENCODE` installs on a monitor.
//!
//! [`AuSection`] adopts the two handle values the host duplicated into this process and owns
//! them from then on: `Drop` unmaps and closes, whatever the session's outcome, because the
//! host does not reap after an IOCTL that completed successfully. Every field past the
//! host-stamped layout is written through atomic views over the mapping — the encode thread
//! is the only writer, the host reads under the `latest` token's generation check.

use std::collections::VecDeque;
use std::mem::offset_of;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use pf_driver_proto::encode::au::{self, AuHeader, AuSlot};
use pf_driver_proto::encode::{FrameToken, SetEncodeRequest};
use pf_umdf_util::section::{self, MappedView};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::SetEvent;

use super::convert::Fail;
use super::thread::EncodeThread;
use crate::worker::OwnedHandle;

/// The mapped section and the ready event, owned. The view is bounds-checked
/// ([`MappedView`]); the section handle is closed once the view holds the section. `Send` and
/// `Sync` come from its fields: [`MappedView`] and [`OwnedHandle`] prove their own.
pub struct AuSection {
    view: MappedView,
    event: OwnedHandle,
    /// Host-stamped heap bounds, validated once at map time.
    heap_offset: u32,
    heap_bytes: u32,
}

impl AuSection {
    /// Map `section` and adopt both handles. `Err` means NOTHING was adopted: the values are
    /// left for the host to reap alongside the IOCTL's failure status.
    pub fn map(section: u64, event: u64, section_bytes: u32) -> Result<Self, Fail> {
        // The header read below needs the whole fixed layout to be there: a short section is
        // refused before anything is mapped, not faulted on.
        if (section_bytes as usize) < au::HEAP_OFFSET {
            dbglog!("[pf-vd] encode: AU section of {section_bytes} B is shorter than its layout");
            return Err((-9, "section"));
        }
        let Some(view) = MappedView::from_handle_value(section, section_bytes as usize) else {
            dbglog!(
                "[pf-vd] encode: MapViewOfFile({section_bytes} B) failed: {:?}",
                windows::core::Error::from_win32()
            );
            return Err((-9, "map"));
        };
        let mut raw = [0u8; au::AU_HEADER_SIZE];
        view.read_bytes(0, &mut raw);
        let header: AuHeader = bytemuck::pod_read_unaligned(&raw);
        let fits =
            au::au_readable(&header) && au::section_bytes(header.heap_bytes) <= section_bytes;
        if !fits {
            dbglog!("[pf-vd] encode: AU section unreadable: {header:?} in {section_bytes} B");
            // Dropping `view` unmaps; the handles stay the host's to reap with the failure.
            return Err((-9, "section"));
        }
        // The view keeps the section alive, so the duplicated handle can close now; the event
        // stays open for `publish_latest`.
        section::close_handle_value(section);
        // SAFETY: `event` is the duplicated ready event; this value is its sole closer.
        let event =
            unsafe { OwnedHandle::from_raw(HANDLE(event as usize as *mut core::ffi::c_void)) };
        Ok(Self {
            view,
            event,
            heap_offset: header.heap_offset,
            heap_bytes: header.heap_bytes,
        })
    }

    /// `(offset, bytes)` of the heap inside the section.
    pub fn heap(&self) -> (u32, u32) {
        (self.heap_offset, self.heap_bytes)
    }

    fn u32_at(&self, off: usize) -> &AtomicU32 {
        self.view.atomic_u32(off)
    }

    fn u64_at(&self, off: usize) -> &AtomicU64 {
        self.view.atomic_u64(off)
    }

    /// Slot `i`'s state word.
    pub fn slot_state(&self, i: usize) -> &AtomicU32 {
        self.u32_at(au::slot_offset(i) + offset_of!(AuSlot, state))
    }

    /// Write slot `i`'s record, state last with Release, so a reader that Acquire-loads
    /// `PUBLISHED` sees the fields that belong to these bytes.
    pub fn publish_slot(&self, i: usize, slot: &AuSlot) {
        let base = au::slot_offset(i);
        self.u32_at(base + offset_of!(AuSlot, offset))
            .store(slot.offset, Ordering::Relaxed);
        self.u32_at(base + offset_of!(AuSlot, len))
            .store(slot.len, Ordering::Relaxed);
        self.u32_at(base + offset_of!(AuSlot, wire_seq))
            .store(slot.wire_seq, Ordering::Relaxed);
        self.u32_at(base + offset_of!(AuSlot, source_seq))
            .store(slot.source_seq, Ordering::Relaxed);
        self.u64_at(base + offset_of!(AuSlot, qpc_pts))
            .store(slot.qpc_pts, Ordering::Relaxed);
        self.u32_at(base + offset_of!(AuSlot, flags))
            .store(slot.flags, Ordering::Relaxed);
        self.u64_at(base + offset_of!(AuSlot, qpc_submit))
            .store(slot.qpc_submit, Ordering::Relaxed);
        self.u64_at(base + offset_of!(AuSlot, qpc_published))
            .store(slot.qpc_published, Ordering::Relaxed);
        self.slot_state(i).store(au::PUBLISHED, Ordering::Release);
    }

    /// Copy `bytes` into the heap at section offset `offset`. `false` — nothing written — for a
    /// range outside the heap, which no reservation the ring allocator hands out can be.
    #[must_use]
    pub fn write_heap(&self, offset: u32, bytes: &[u8]) -> bool {
        let start = offset as usize;
        let end = start + bytes.len();
        let heap_end = self.heap_offset as usize + self.heap_bytes as usize;
        if start < self.heap_offset as usize || end > heap_end {
            return false;
        }
        // The encode thread is the only writer; the host reads only slots it Acquire-loaded.
        self.view.copy_from_slice(start, bytes);
        true
    }

    /// Store the publish token (Release, after the slot) and wake the host.
    pub fn publish_latest(&self, token: FrameToken) {
        self.u64_at(offset_of!(AuHeader, latest))
            .store(token.pack(), Ordering::Release);
        // SAFETY: `event` is the live host-created ready event this section owns.
        unsafe {
            let _ = SetEvent(self.event.as_raw());
        }
    }

    pub fn store_u32(&self, off: usize, v: u32) {
        self.u32_at(off).store(v, Ordering::Relaxed);
    }

    pub fn store_u64(&self, off: usize, v: u64) {
        self.u64_at(off).store(v, Ordering::Relaxed);
    }

    pub fn add_u32(&self, off: usize, n: u32) -> u32 {
        self.u32_at(off).fetch_add(n, Ordering::Relaxed) + n
    }

    pub fn add_u64(&self, off: usize, n: u64) -> u64 {
        self.u64_at(off).fetch_add(n, Ordering::Relaxed) + n
    }
}

/// One `ENCODE_CTL` op for the encode thread, drained between frames. One-shot: the host
/// sends the next only after this IOCTL returned, so the queue never holds more than a few.
#[derive(Clone, Copy, Debug)]
pub enum Ctl {
    RequestKeyframe,
    /// Wire indexes `first..=last`.
    InvalidateRefFrames(u32, u32),
    DistrustReferences,
    /// kbps.
    ReconfigureBitrate(u32),
    /// `pf_frame::HdrMeta` as its 28 bytes.
    SetHdrMeta([u8; 28]),
    Flush,
}

/// One monitor's live encode: the request it was opened from, the section it publishes into
/// and the thread doing it. The monitor holds one `Arc`; the encode thread holds another for
/// as long as it runs, so a detached thread keeps the section mapped until it really exits.
pub struct EncodeSession {
    pub request: SetEncodeRequest,
    pub section: AuSection,
    /// Stamped into every publish token; bumped per `SET_ENCODE` by the monitor.
    pub generation: u32,
    /// The first `wire_seq` the current thread stamps ([`crate::encode::thread`]).
    pub wire_seq_base: AtomicU32,
    /// The drain worker saw a different device epoch than the pool was built on (TDR): frames
    /// stop, the host's next `SET_ENCODE` rebuilds on the new device.
    pub stale: AtomicBool,
    /// The control mailbox ([`Ctl`]); the pool event wakes the thread to drain it.
    pub ctl: Mutex<VecDeque<Ctl>>,
    thread: Mutex<Option<EncodeThread>>,
}

impl EncodeSession {
    pub fn new(request: SetEncodeRequest, section: AuSection, generation: u32) -> Self {
        section.store_u32(offset_of!(AuHeader, generation), generation);
        section.store_u32(offset_of!(AuHeader, wire_seq_base), request.wire_seq_base);
        section.store_u32(offset_of!(AuHeader, encoder_state), au::ENCODER_CLOSED);
        Self {
            request,
            section,
            generation,
            wire_seq_base: AtomicU32::new(request.wire_seq_base),
            stale: AtomicBool::new(false),
            ctl: Mutex::new(VecDeque::new()),
            thread: Mutex::new(None),
        }
    }

    /// Queue one op for the thread; the caller wakes it.
    pub fn push_ctl(&self, op: Ctl) {
        crate::registry::lock(&self.ctl).push_back(op);
    }

    /// Everything queued, in order.
    pub fn take_ctl(&self) -> Vec<Ctl> {
        crate::registry::lock(&self.ctl).drain(..).collect()
    }

    /// Install the running thread; whatever it displaces is handed back to stop with no lock
    /// held.
    #[must_use]
    pub fn set_thread(&self, thread: EncodeThread) -> Option<EncodeThread> {
        crate::registry::lock(&self.thread).replace(thread)
    }

    /// Take the thread out; the caller stops it with no lock held.
    #[must_use]
    pub fn take_thread(&self) -> Option<EncodeThread> {
        crate::registry::lock(&self.thread).take()
    }

    /// Stop the thread within [`EncodeThread::STOP_BOUND`], detaching it if it will not.
    pub fn stop(&self) {
        if let Some(t) = self.take_thread() {
            t.stop(&self.section);
        }
    }
}
