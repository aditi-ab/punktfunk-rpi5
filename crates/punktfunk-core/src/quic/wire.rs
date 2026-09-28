//! Little-endian field framing for the control messages (`CTL_MAGIC ‖ type`) and the
//! tagged datagram planes. [`Wr`] appends fields in wire order; [`Rd`] checks the header
//! and length once, then takes fields in the same order. Optional tails stay explicit
//! in each message's decoder.

use super::CTL_MAGIC;
use crate::error::{PunktfunkError, Result};
use std::ops::RangeBounds;

/// Builds one message: `Wr::ctl(MSG_X, len).u32(a).u16(b).done()`.
pub(super) struct Wr(Vec<u8>);

impl Wr {
    /// `CTL_MAGIC ‖ msg`, with room for a `cap`-byte message.
    pub(super) fn ctl(msg: u8, cap: usize) -> Wr {
        let mut b = Vec::with_capacity(cap);
        b.extend_from_slice(CTL_MAGIC);
        b.push(msg);
        Wr(b)
    }

    /// One tag byte, with room for a `cap`-byte datagram.
    pub(super) fn tag(tag: u8, cap: usize) -> Wr {
        let mut b = Vec::with_capacity(cap);
        b.push(tag);
        Wr(b)
    }

    pub(super) fn u8(mut self, v: u8) -> Wr {
        self.0.push(v);
        self
    }

    pub(super) fn u16(self, v: u16) -> Wr {
        self.bytes(&v.to_le_bytes())
    }

    pub(super) fn u32(self, v: u32) -> Wr {
        self.bytes(&v.to_le_bytes())
    }

    pub(super) fn u64(self, v: u64) -> Wr {
        self.bytes(&v.to_le_bytes())
    }

    pub(super) fn bytes(mut self, v: &[u8]) -> Wr {
        self.0.extend_from_slice(v);
        self
    }

    pub(super) fn done(self) -> Vec<u8> {
        self.0
    }
}

/// Reads one message whose header and length [`Rd::ctl`] / [`Rd::tag`] already checked.
/// A fixed field read past the checked length panics: that is a codec bug, not bad input.
pub(super) struct Rd<'a> {
    b: &'a [u8],
    off: usize,
}

impl<'a> Rd<'a> {
    /// A control message of type `msg` whose length is in `lens` (at least 5). `what`
    /// names the message in the error.
    pub(super) fn ctl(
        b: &'a [u8],
        msg: u8,
        lens: impl RangeBounds<usize>,
        what: &'static str,
    ) -> Result<Rd<'a>> {
        if !lens.contains(&b.len()) || b.len() < 5 || &b[0..4] != CTL_MAGIC || b[4] != msg {
            return Err(PunktfunkError::InvalidArg(what));
        }
        Ok(Rd { b, off: 5 })
    }

    /// A datagram tagged `tag` of at least `min_len` bytes.
    pub(super) fn tag(b: &'a [u8], tag: u8, min_len: usize) -> Option<Rd<'a>> {
        (b.len() >= min_len.max(1) && b[0] == tag).then_some(Rd { b, off: 1 })
    }

    fn take<const N: usize>(&mut self) -> [u8; N] {
        let v = self.b[self.off..self.off + N].try_into().unwrap();
        self.off += N;
        v
    }

    pub(super) fn u8(&mut self) -> u8 {
        u8::from_le_bytes(self.take())
    }

    pub(super) fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.take())
    }

    pub(super) fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take())
    }

    pub(super) fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take())
    }

    pub(super) fn bytes(&mut self, n: usize) -> &'a [u8] {
        let v = &self.b[self.off..self.off + n];
        self.off += n;
        v
    }

    /// The next byte, or `None` at the end: an optional one-byte tail.
    pub(super) fn opt_u8(&mut self) -> Option<u8> {
        (self.remaining() >= 1).then(|| self.u8())
    }

    /// The next two bytes, or `None` short of them: an optional `u16` tail.
    pub(super) fn opt_u16(&mut self) -> Option<u16> {
        (self.remaining() >= 2).then(|| self.u16())
    }

    pub(super) fn remaining(&self) -> usize {
        self.b.len() - self.off
    }

    /// Everything after the fields read so far.
    pub(super) fn rest(self) -> &'a [u8] {
        &self.b[self.off..]
    }
}
