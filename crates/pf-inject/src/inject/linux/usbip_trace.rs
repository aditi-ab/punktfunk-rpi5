//! Byte-level tracing for the USB/IP transport (`PUNKTFUNK_USBIP_TRACE`).
//!
//! # Why this exists
//!
//! A USB/IP connection is a *framed byte stream over one TCP socket*, and every frame's length is
//! declared inside the frame. So any reply that writes a different number of bytes than its header
//! declares does not corrupt that one URB — it shifts every byte after it, and the peer's next read
//! lands mid-frame. `vhci_hcd` reports the wreckage from wherever it happens to notice
//! (`recv xbuf`, `unknown pdu`, `cannot find a urb of seqnum`), which is never where the extra or
//! missing bytes were written. Reading the code cannot settle it, because the bug *is* a
//! disagreement between the code's arithmetic and the wire.
//!
//! This wraps the socket and writes both directions to disk verbatim, plus a record of where each
//! read/write call began and ended, so [`crate::usbip_trace`]'s companion analyser can walk the
//! streams as PDUs and name the first frame whose declared length and written length disagree.
//!
//! It is off unless `PUNKTFUNK_USBIP_TRACE` is set, and it is deliberately dumb: no parsing, no
//! filtering, no allocation beyond the write buffer. A tracer that interprets can be wrong in the
//! same way the code under test is wrong.
//!
//! # Using it
//!
//! ```text
//! PUNKTFUNK_USBIP_TRACE=/tmp/pad punktfunk-host pad-usbip-test --seconds 5
//! ```
//!
//! yields `/tmp/pad.<label>.rx` (kernel → us), `.tx` (us → kernel) and `.idx` (one
//! `us,dir,offset,len` record per call). Feed them to `scripts/usbip-trace-analyse.py`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Where the trace goes, if tracing is on. One prefix per attached device.
pub fn trace_prefix(label: &str) -> Option<String> {
    let base = std::env::var("PUNKTFUNK_USBIP_TRACE").ok()?;
    if base.is_empty() || base == "0" {
        return None;
    }
    // `label` is a human string ("virtual DualSense 0"); keep it filesystem-safe.
    let safe: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Some(format!("{base}.{safe}"))
}

/// The three files a trace is made of, shared by the read and write halves.
struct Sink {
    rx: BufWriter<File>,
    tx: BufWriter<File>,
    idx: BufWriter<File>,
    rx_off: u64,
    tx_off: u64,
    start: Instant,
}

impl Sink {
    fn create(prefix: &str) -> std::io::Result<Self> {
        // Append rather than `with_extension`, which would treat the label's own dots/dashes as an
        // extension and collapse every pad's trace onto the same three files.
        let f = |ext: &str| File::create(format!("{prefix}.{ext}"));
        Ok(Sink {
            rx: BufWriter::new(f("rx")?),
            tx: BufWriter::new(f("tx")?),
            idx: BufWriter::new(f("idx")?),
            rx_off: 0,
            tx_off: 0,
            start: Instant::now(),
        })
    }

    /// Record one completed call. `dir` is `r` (kernel → us) or `w` (us → kernel).
    fn record(&mut self, dir: char, bytes: &[u8]) {
        let (stream, off) = match dir {
            'r' => (&mut self.rx, &mut self.rx_off),
            _ => (&mut self.tx, &mut self.tx_off),
        };
        let at = *off;
        let _ = stream.write_all(bytes);
        *off += bytes.len() as u64;
        let us = self.start.elapsed().as_micros();
        let _ = writeln!(self.idx, "{us},{dir},{at},{}", bytes.len());
        // Flushed per call on purpose: the failure under investigation ends with the process's
        // socket dying, and a buffered tail is exactly the part that would be lost.
        let _ = stream.flush();
        let _ = self.idx.flush();
    }
}

/// An opened set of trace files, ready to wrap a stream.
///
/// Opening is separate from wrapping so the caller can fall back to the untraced path without
/// having already surrendered its socket to a constructor that then failed.
pub struct TraceSink(Arc<Mutex<Sink>>);

/// Open `<prefix>.rx` / `.tx` / `.idx`, truncating any previous trace.
pub fn open_trace(prefix: &str) -> std::io::Result<TraceSink> {
    Ok(TraceSink(Arc::new(Mutex::new(Sink::create(prefix)?))))
}

/// A socket wrapper that copies both directions to disk.
///
/// Wraps at the *call site* rather than inside the vendored server, so the vendored crate carries
/// no debug scaffolding and the traced and untraced paths run the identical handler.
pub struct TracedIo<T> {
    inner: T,
    sink: Arc<Mutex<Sink>>,
}

impl<T> TracedIo<T> {
    /// Begin copying `inner`'s traffic into an already-opened [`TraceSink`].
    pub fn wrap(inner: T, sink: TraceSink) -> Self {
        TracedIo {
            inner,
            sink: sink.0,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for TracedIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &r {
            let got = buf.filled()[before..].to_vec();
            // A zero-length ready read is EOF, and it is worth a record of its own: it is the
            // moment the peer went away, and which side went first is the whole question.
            if let Ok(mut s) = self.sink.lock() {
                s.record('r', &got);
            }
        }
        r
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for TracedIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &r {
            if let Ok(mut s) = self.sink.lock() {
                s.record('w', &buf[..*n]);
            }
        }
        r
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
