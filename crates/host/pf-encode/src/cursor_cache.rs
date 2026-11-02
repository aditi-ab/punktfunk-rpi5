//! Which cursor bitmap an encoder holds, and when the next frame uploads it again.
//!
//! Pure: no `ash`, no OS types, so the table below runs on every target. Five sites hold a
//! [`CursorCache`] each: NVENC's slot blend and its convert worker, the per-slot CSC plane
//! Vulkan Video and PyroWave share, the PyroWave proxy and its worker. A site asks
//! [`CursorCache::decide`] before it uploads, calls [`CursorCache::forget`] when the upload it
//! was told to make never ran, and [`CursorCache::rebuild`] when its session dropped what it
//! held.
//!
//! The key is the bitmap's serial and its PQ re-encode. A depth or device change reaches the
//! cache as a `rebuild`. Each row of `cursor_table` names the rule it pins; [`copy_rows`] lands
//! a bitmap wider than its plane. Design: `design/pf-encode-overhaul.md` §4.4.

/// What an uploaded bitmap was made from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    /// [`pf_frame::CursorOverlay::serial`]: bumps when the bitmap changes. Any value is real.
    pub(crate) serial: u64,
    /// The bitmap went up re-encoded as PQ.
    pub(crate) pq: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Upload,
    Keep,
}

/// The key whose bitmap one site holds: `None` before the first upload, after a forget and
/// after a rebuild.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CursorCache {
    uploaded: Option<Key>,
}

impl CursorCache {
    /// `Keep` when `key` is the one held. Otherwise `Upload`, and `key` is held from now on:
    /// the caller uploads, and forgets if that upload does not run.
    pub(crate) fn decide(&mut self, key: Key) -> Verdict {
        if self.uploaded == Some(key) {
            return Verdict::Keep;
        }
        self.uploaded = Some(key);
        Verdict::Upload
    }

    /// The upload `decide` asked for never ran: a discarded record, a refused frame.
    pub(crate) fn forget(&mut self) {
        self.uploaded = None;
    }

    /// The session dropped what it held: a rebuilt session, a cleared or respawned worker.
    pub(crate) fn rebuild(&mut self) {
        self.uploaded = None;
    }
}

/// Fill `dst` row by row (`row` bytes each) from a bitmap whose rows are `src_row` apart: a
/// wider bitmap lands as its top-left square, not as a sheared linear prefix.
pub(crate) fn copy_rows(dst: &mut [u8], src: &[u8], row: usize, src_row: usize) {
    for (d, s) in dst.chunks_exact_mut(row).zip(src.chunks_exact(src_row)) {
        d.copy_from_slice(&s[..row]);
    }
}

#[cfg(test)]
mod tests {
    use super::Verdict::{Keep, Upload};
    use super::*;

    /// One step on cache `slot` of a row's two.
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// Decide the bitmap `serial`, PQ when `pq`, and expect the verdict.
        Decide(usize, u64, bool, Verdict),
        Forget(usize),
        Rebuild(usize),
    }
    use Step::{Decide, Forget, Rebuild};

    /// A row: the rule it pins and its steps. Every row starts from two empty caches.
    type Row = (&'static str, Vec<Step>);

    #[test]
    fn cursor_table() {
        let rows: Vec<Row> = vec![
            (
                "upload the cursor again after a session rebuild flips its PQ encode",
                vec![
                    Decide(0, 7, true, Upload),
                    Decide(0, 7, true, Keep),
                    Rebuild(0),
                    Decide(0, 7, false, Upload),
                    Decide(0, 7, false, Keep),
                ],
            ),
            (
                "a depth flip without a rebuild re-encodes the bitmap",
                vec![
                    Decide(0, 7, true, Upload),
                    Decide(0, 7, false, Upload),
                    Decide(0, 7, false, Keep),
                ],
            ),
            (
                "forget a cursor upload a failed frame discarded",
                vec![
                    Decide(0, 1, false, Upload),
                    Decide(0, 2, false, Upload),
                    Forget(0),
                    Decide(0, 2, false, Upload),
                    Decide(0, 2, false, Keep),
                ],
            ),
            (
                "re-upload a cursor a failed record dropped, serial 0 included",
                vec![
                    Decide(0, 0, false, Upload),
                    Forget(0),
                    Decide(0, 0, false, Upload),
                    Decide(0, 0, false, Keep),
                ],
            ),
            (
                "forget a cursor upload the slot never ran, on a frame that kept it too",
                vec![
                    Decide(0, 7, false, Upload),
                    Decide(0, 7, false, Keep),
                    Forget(0),
                    Decide(0, 7, false, Upload),
                ],
            ),
            (
                "re-upload the cursor after a session rebuild cleared the worker",
                vec![
                    Decide(0, 7, false, Upload),
                    Rebuild(0),
                    Decide(0, 7, false, Upload),
                    Decide(0, 7, false, Keep),
                ],
            ),
            (
                "one blend rule for every payload: the bitmap uploads once per session",
                vec![
                    Decide(0, 7, false, Upload),
                    Decide(0, 7, false, Keep),
                    Decide(0, 7, false, Keep),
                ],
            ),
            (
                "a respawned worker holds no bitmap",
                vec![
                    Decide(0, 7, false, Upload),
                    Rebuild(0),
                    Decide(0, 8, false, Upload),
                    Decide(0, 8, false, Keep),
                ],
            ),
            (
                "a serial change while a frame is in flight uploads once per slot",
                vec![
                    Decide(0, 1, false, Upload),
                    Decide(1, 2, false, Upload),
                    Decide(0, 2, false, Upload),
                    Decide(1, 2, false, Keep),
                    Decide(0, 2, false, Keep),
                ],
            ),
            (
                "an empty bitmap still counts as sent: the serial decides, not the pixels",
                vec![
                    Decide(0, 7, false, Upload),
                    Decide(0, 7, false, Keep),
                    Decide(0, 8, false, Upload),
                ],
            ),
        ];
        for (name, steps) in rows {
            let mut caches = [CursorCache::default(); 2];
            for (i, step) in steps.into_iter().enumerate() {
                match step {
                    Decide(slot, serial, pq, want) => assert_eq!(
                        caches[slot].decide(Key { serial, pq }),
                        want,
                        "{name}: step {i} {step:?}"
                    ),
                    Forget(slot) => caches[slot].forget(),
                    Rebuild(slot) => caches[slot].rebuild(),
                }
            }
        }
    }

    #[test]
    fn a_wider_bitmap_lands_as_its_top_left_square() {
        // Three pixels wide into a two-wide plane: plane row 2 starts where bitmap row 2 does.
        let src: Vec<u8> = (0..3 * 2 * 4).map(|i| i as u8).collect();
        let mut dst = [0u8; 2 * 2 * 4];
        copy_rows(&mut dst, &src, 2 * 4, 3 * 4);
        assert_eq!(&dst[..8], &src[..8]);
        assert_eq!(&dst[8..], &src[12..20]);
    }
}
