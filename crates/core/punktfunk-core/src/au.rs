//! Memory an access unit is assembled in. The reassembler writes each shard straight into
//! an [`AuBuf`], which becomes [`Frame::data`](crate::session::Frame::data); dropping the
//! frame hands the memory back to whoever lent it. The default lender is a heap pool. An
//! embedder whose decoder reads the AU from its own memory (a GPU-visible buffer) installs
//! an [`AuMemory`] with [`Session::set_au_memory`](crate::session::Session::set_au_memory).

use std::sync::{Arc, Mutex};

/// Bytes an AU lives in. Dropping the store returns it to its lender.
pub trait AuStore: Send + std::any::Any {
    /// The whole region; an AU uses a prefix of it.
    fn bytes(&self) -> &[u8];
    fn bytes_mut(&mut self) -> &mut [u8];
}

/// Lends AU memory to the reassembler.
pub trait AuMemory: Send + Sync {
    /// A store of at least `len` bytes, contents unspecified, or `None` for the heap.
    fn take(&self, len: usize) -> Option<Box<dyn AuStore>>;
}

/// One AU's bytes: the first `len` bytes of a store.
pub struct AuBuf {
    store: Box<dyn AuStore>,
    len: usize,
}

impl AuBuf {
    /// `len` bytes of a store from `memory`, or of the heap when it has none. Contents
    /// unspecified: the caller writes every byte it hands on.
    pub fn take(memory: &dyn AuMemory, len: usize) -> AuBuf {
        let store = memory
            .take(len)
            .filter(|s| s.bytes().len() >= len)
            .unwrap_or_else(|| Box::new(HeapStore::new(vec![0; len], None)));
        AuBuf { store, len }
    }

    /// The store, for a decoder that reads the AU where it lies.
    pub fn store(&self) -> &dyn AuStore {
        &*self.store
    }

    pub fn store_mut(&mut self) -> &mut dyn AuStore {
        &mut *self.store
    }

    /// Grow or shrink to `len` bytes. Growth past the store moves to a larger one from
    /// `memory`, copying what is there; bytes past the old length are unspecified.
    pub(crate) fn resize(&mut self, len: usize, memory: &dyn AuMemory) {
        if len > self.store.bytes().len() {
            let mut bigger = AuBuf::take(memory, len);
            bigger.store.bytes_mut()[..self.len].copy_from_slice(&self[..]);
            *self = bigger;
        }
        self.len = len;
    }

    /// Shrink to `len` bytes; never grows.
    pub(crate) fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }
}

impl Default for AuBuf {
    /// Empty, on the heap: what `mem::take` leaves in a frame.
    fn default() -> AuBuf {
        Vec::new().into()
    }
}

impl From<Vec<u8>> for AuBuf {
    fn from(v: Vec<u8>) -> AuBuf {
        let len = v.len();
        AuBuf {
            store: Box::new(HeapStore::new(v, None)),
            len,
        }
    }
}

impl std::ops::Deref for AuBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.store.bytes()[..self.len]
    }
}

impl std::ops::DerefMut for AuBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        let len = self.len;
        &mut self.store.bytes_mut()[..len]
    }
}

impl Clone for AuBuf {
    /// A heap copy, wherever the original lives.
    fn clone(&self) -> AuBuf {
        self.to_vec().into()
    }
}

impl PartialEq<[u8]> for AuBuf {
    fn eq(&self, other: &[u8]) -> bool {
        **self == *other
    }
}

impl PartialEq<Vec<u8>> for AuBuf {
    fn eq(&self, other: &Vec<u8>) -> bool {
        **self == **other
    }
}

impl PartialEq<&[u8]> for AuBuf {
    fn eq(&self, other: &&[u8]) -> bool {
        **self == **other
    }
}

impl std::fmt::Debug for AuBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuBuf").field("len", &self.len).finish()
    }
}

/// AU buffers the heap pool keeps between frames: a few in flight, one being decoded.
const HEAP_KEEP: usize = 8;

/// The default lender: heap buffers, reused.
#[derive(Default)]
pub struct HeapPool {
    free: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl AuMemory for HeapPool {
    fn take(&self, len: usize) -> Option<Box<dyn AuStore>> {
        let mut v = self
            .free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .unwrap_or_default();
        if v.len() < len {
            v.resize(len, 0);
        }
        Some(Box::new(HeapStore::new(v, Some(self.free.clone()))))
    }
}

struct HeapStore {
    buf: Vec<u8>,
    home: Option<Arc<Mutex<Vec<Vec<u8>>>>>,
}

impl HeapStore {
    fn new(buf: Vec<u8>, home: Option<Arc<Mutex<Vec<Vec<u8>>>>>) -> HeapStore {
        HeapStore { buf, home }
    }
}

impl AuStore for HeapStore {
    fn bytes(&self) -> &[u8] {
        &self.buf
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.buf
    }
}

impl Drop for HeapStore {
    fn drop(&mut self) {
        if let Some(home) = &self.home {
            let mut free = home.lock().unwrap_or_else(|e| e.into_inner());
            if free.len() < HEAP_KEEP {
                free.push(std::mem::take(&mut self.buf));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_buffers_come_back_and_grow_in_place() {
        let pool = HeapPool::default();
        let mut a = AuBuf::take(&pool, 8);
        a.copy_from_slice(b"abcdefgh");
        let ptr = a.as_ptr();
        drop(a);
        let b = AuBuf::take(&pool, 4);
        assert_eq!(b.as_ptr(), ptr, "the dropped buffer is reused");
        assert_eq!(b.len(), 4);
        let mut c = AuBuf::from(b"xy".to_vec());
        c.resize(5, &pool);
        assert_eq!(&c[..2], b"xy", "growth keeps what was written");
        assert_eq!(c.len(), 5);
        c.resize(1, &pool);
        assert_eq!(&c[..], b"x");
    }
}
