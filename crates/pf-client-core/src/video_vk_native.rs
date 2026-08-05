//! Native Vulkan Video H.264 decode backend (WP-C of the native-decode program):
//! pf-vkdecode's [`VkH264Decoder`] running on the PRESENTER's own VkDevice — the same
//! zero-copy shape as the FFmpeg-Vulkan backend, with no FFmpeg in the path. Strictly
//! the `PUNKTFUNK_DECODER=native-vulkan` runtime opt-in (`video::native_vulkan_gate`);
//! the automatic ladder stays FFmpeg's until WP-D's A/B verdict.
//!
//! **Queue lock:** pf-vkdecode submits on queue 0 of the decode family
//! ([`DECODE_QUEUE_INDEX`] — the presenter creates exactly one queue per family). When
//! the decode family IS the presenter's graphics family, that is the very `VkQueue` the
//! presenter/Skia/overlay submit and present on, so every decode submit must hold the
//! device's shared [`video::QueueLock`] (`vkQueueSubmit` external sync — the 2026-07-09
//! `VK_ERROR_DEVICE_LOST` class). When the families differ, the decode queue has exactly
//! one submitter (this backend, on the pump thread) and locking would serialize decode
//! against present for nothing — [`submit_queues_collide`] is the whole decision. (The
//! FFmpeg path locks on every family only because `lock_queue` is one callback pair for
//! the whole device; the collision it exists to prevent is the shared-queue one.)
//!
//! **Release lifecycle** (decode → present → retire → release): each delivered frame
//! ships as a [`NativeVkFrame`] whose [`NativeReleaseGuard`] sends a token (seq +
//! generation) into this backend's channel on drop. The presenter drops the frame only
//! after the sampling submission's fence has been waited (its retired-frame slot), so a
//! returned token proves the GPU is done with the image; a frame dropped UNPRESENTED
//! (newest-wins displacement, post-demotion drain) releases through the same drop. The
//! backend drains the channel at every `decode` entry and calls
//! [`VkH264Decoder::release_frame`] — but only once the frame's decode-status query has
//! also been read (the slot stays pinned meanwhile, which is what makes re-polling the
//! query safe: an unreleased slot can never be recycled under the poll).
//!
//! **Status queries:** every decode op carries a `RESULT_STATUS_ONLY` query —
//! [`VkH264Decoder::poll_status`], read non-blockingly here at each decode entry. A
//! `Failed` verdict is driver-reported decode corruption, the class FFmpeg's
//! `vulkan_decode.c` (`nb_queries = 0`) architecturally cannot see — the Xbox Ally X
//! field case. It surfaces as an `Err` from the CURRENT `decode_frame` call so the
//! existing streak/reanchor machinery fires exactly as it does for FFmpeg errors.
//!
//! **Teardown:** dropping this backend (demotion, session end) waits — bounded — for
//! every shipped frame's token before dropping the decoder, because the decoder's Drop
//! destroys the pool images and its own drain only covers DECODE work, not the
//! presenter's in-flight sampling. Tokens arrive as the presenter's fence waits/drops
//! displace the frames; a presenter wedged past [`TEARDOWN_BUDGET`] forfeits (warned).

use crate::video::{
    ColorDesc, NativeReleaseGuard, NativeReleaseToken, NativeVkFrame, NativeVkLayout,
    VulkanDecodeDevice,
};
use anyhow::{anyhow, bail, Result};
use pf_vkdecode::ash::vk;
use pf_vkdecode::ash::vk::Handle as _;
use pf_vkdecode::{DecodeStatus, DecodedVkFrame, DeviceHandles, VkH264Decoder};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The queue index this backend submits on within the decode family: the presenter
/// creates exactly ONE queue (index 0) per family it enables (`vk/setup.rs` — one
/// `VkDeviceQueueCreateInfo` per family, `queue_count = 1`), so 0 is the only queue
/// that exists.
const DECODE_QUEUE_INDEX: u32 = 0;

/// Teardown budget for the presenter to hand back every outstanding frame token (its
/// next present's fence wait, typically one frame). Generous against a paused stream,
/// finite against a wedged presenter — after this the pools are destroyed anyway
/// (warned; the realistic residue is a logically-held frame, not in-flight GPU work).
const TEARDOWN_BUDGET: Duration = Duration::from_millis(500);

/// Query-poll belt: a frame whose token has returned had its decode op complete on the
/// GPU (the presenter's submit waited the decode timeline), so its status query MUST be
/// readable — if it still reads Pending after this many polls, give the slot back
/// anyway rather than strand it (debug-logged; the status is then simply unknown).
const MAX_POLLS_AFTER_RELEASE: u32 = 3;

/// Do the presenter's and the decoder's submit queues collide? Both sides use queue
/// index 0 of their family by construction (the presenter's graphics queue is
/// `get_device_queue(qfi, 0)`, the decoder's is [`DECODE_QUEUE_INDEX`] of `decode_qf`),
/// so the collision test is family equality. Pure — the queue-lock decision is
/// CPU-testable.
fn submit_queues_collide(graphics_qf: u32, decode_qf: u32) -> bool {
    graphics_qf == decode_qf
}

/// [`pf_vkdecode::QueueLock`] over the device's shared [`crate::video::QueueLock`] —
/// or over nothing, when the decode queue provably has no other submitter (see the
/// module doc's queue-lock section).
enum NativeQueueLock {
    /// Decode shares the presenter's graphics queue: serialize with everyone.
    Shared(std::sync::Arc<crate::video::QueueLock>),
    /// A separate decode family/queue: this backend is its only submitter.
    Uncontended,
}

impl pf_vkdecode::QueueLock for NativeQueueLock {
    fn lock(&self) {
        if let NativeQueueLock::Shared(l) = self {
            l.lock();
        }
    }
    fn unlock(&self) {
        if let NativeQueueLock::Shared(l) = self {
            l.unlock();
        }
    }
}

/// One frame shipped to the presenter and not yet fully settled: settled = its release
/// token came back (GPU reads proven done) AND its status query was read.
struct Shipped {
    seq: u64,
    frame: DecodedVkFrame,
    /// The presenter (or a drop on the way there) returned the token.
    released: bool,
    /// The token said the sampling submission (with its `value + 1` timeline
    /// signal) was enqueued — forwarded to `release_frame` so the decoder waits
    /// the write-back before reusing the image.
    presented: bool,
    /// The status query read a conclusive verdict (or the poll belt expired).
    resolved: bool,
    /// Polls attempted after the token returned — see [`MAX_POLLS_AFTER_RELEASE`].
    polls_after_release: u32,
}

/// Mark the shipped entry a token names as released. Returns false when nothing
/// matches (a late token from before a demotion drain — benign). Pure bookkeeping,
/// split out so the channel-drain behavior is CPU-testable.
fn note_token(outstanding: &mut [Shipped], token: NativeReleaseToken) -> bool {
    match outstanding.iter_mut().find(|s| s.seq == token.seq) {
        Some(s) => {
            debug_assert_eq!(
                s.frame.generation, token.generation,
                "a token's generation always matches the frame it rode on"
            );
            s.released = true;
            s.presented = token.presented;
            true
        }
        None => false,
    }
}

/// The native backend: the decoder plus the shipped-frame ledger and release channel.
pub(crate) struct NativeVulkanDecoder {
    dec: VkH264Decoder,
    /// Cloned into every shipped frame's guard. `Option` so teardown can DROP the
    /// backend's own sender: only then does `release_rx` report Disconnected once
    /// the last guard is gone — the teardown short-circuit signal.
    release_tx: Option<mpsc::Sender<NativeReleaseToken>>,
    release_rx: mpsc::Receiver<NativeReleaseToken>,
    /// Display-ready frames not yet handed to the pump (burst outputs — decode
    /// delivers one per call; the rest wait here, oldest first).
    deliverable: std::collections::VecDeque<DecodedVkFrame>,
    outstanding: Vec<Shipped>,
    next_seq: u64,
}

// SAFETY: the decoder is used strictly serially through `&mut self` from whichever
// single thread owns the enclosing `Decoder` (the session pump) — `Send` only moves
// that ownership. The `Rc`s inside pf-vkdecode's planner never escape it, so they all
// move together; every queue submission runs under the collision-aware queue lock; the
// mpsc endpoints are `Send`. Same contract, same shape as the `VulkanDecoder` and
// `PyroWaveDecoder` impls above/beside it. Deliberately NOT `Sync`.
unsafe impl Send for NativeVulkanDecoder {}

impl NativeVulkanDecoder {
    pub(crate) fn new(vk: &VulkanDecodeDevice) -> Result<NativeVulkanDecoder> {
        if !vk.video_decode {
            bail!("presenter device lacks Vulkan Video decode");
        }
        let lock: Box<dyn pf_vkdecode::QueueLock> =
            if submit_queues_collide(vk.graphics_qf, vk.decode_qf) {
                Box::new(NativeQueueLock::Shared(vk.queue_lock.clone()))
            } else {
                Box::new(NativeQueueLock::Uncontended)
            };
        let handles = DeviceHandles {
            get_instance_proc_addr: vk.get_instance_proc_addr,
            instance: vk.instance,
            physical_device: vk.physical_device,
            device: vk.device,
            decode_qf: vk.decode_qf,
            decode_queue_index: DECODE_QUEUE_INDEX,
            graphics_qf: vk.graphics_qf,
        };
        // SAFETY: the handles are the presenter's live instance/device, which outlives
        // every session pump (the run loop tears the pump — and with it this decoder —
        // down first: the exact liveness contract the FFmpeg and PyroWave backends
        // already rely on over the same bundle). `video_decode` (checked above) is set
        // only when the presenter enabled the Vulkan Video decode extension stack +
        // synchronization2/timelineSemaphore at device creation, and
        // `decode_qf`/`graphics_qf` mirror the families it created queues for (one
        // queue, index 0, each).
        let dec = unsafe { VkH264Decoder::new(&handles, lock) }
            .map_err(|e| anyhow!("VkH264Decoder init: {e}"))?;
        let (release_tx, release_rx) = mpsc::channel();
        Ok(NativeVulkanDecoder {
            dec,
            release_tx: Some(release_tx),
            release_rx,
            deliverable: std::collections::VecDeque::new(),
            outstanding: Vec::new(),
            next_seq: 0,
        })
    }

    /// Feed one complete access unit. `Ok(None)` = no display-ready picture (the pump's
    /// no-output/reanchor machinery reads that exactly as it does for FFmpeg). `Err` =
    /// decode trouble — a decoder error, a plan that needed concealment, or a
    /// driver-reported corrupt PREVIOUS frame — routed through the caller's shared
    /// streak/demotion machinery.
    ///
    /// Ordering: the CURRENT AU decodes FIRST — the planner's reference state must
    /// advance even when a PRIOR frame's status turns out Failed, or the recovery
    /// IDR would land on a decoder that skipped an AU and reports a phantom
    /// reference gap. The prior-frame verdicts are checked after; a corrupt verdict
    /// costs exactly this one AU's output (released unshown), never parser state.
    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<NativeVkFrame>> {
        self.drain_releases();

        let delivered = self.dec.decode(au).map_err(|e| anyhow!("decode: {e}"))?;
        let warnings = self.dec.take_warnings();
        // Everything this AU made display-ready, oldest first (`take_ready` drained
        // so burst outputs are never stranded inside the decoder).
        let mut fresh: Vec<DecodedVkFrame> = Vec::new();
        if let Some(frame) = delivered {
            fresh.push(frame);
        }
        while let Some(frame) = self.dec.take_ready() {
            fresh.push(frame);
        }

        let corrupt = self.settle_statuses();
        if !warnings.is_empty() || corrupt > 0 {
            // Concealment planned into THIS AU, or driver-reported corruption on a
            // PRIOR frame (the Ally X class, invisible to FFmpeg's query-less
            // decoder): this call's output is released unshown and the call errors,
            // arming the reanchor gate — same path, same volume as an FFmpeg
            // reference-miss error (never quieter).
            for frame in fresh {
                if let Err(e) = self.dec.release_frame(&frame, false) {
                    tracing::debug!(error = %e, "releasing an unshown frame failed");
                }
            }
            if corrupt > 0 {
                return Err(anyhow!(
                    "driver reported decode corruption on {corrupt} prior frame(s) \
                     (RESULT_STATUS_ONLY query) — re-anchor needed"
                ));
            }
            tracing::warn!(
                ?warnings,
                "native decode planned with concealment — dropping the frame, \
                 requesting re-anchor"
            );
            bail!(
                "AU planned with concealment ({} warning(s))",
                warnings.len()
            );
        }

        self.deliverable.extend(fresh);
        Ok(self.deliverable.pop_front().map(|frame| self.ship(frame)))
    }

    /// Wrap a delivered [`DecodedVkFrame`] for the presenter and enter it into the
    /// shipped ledger (the original stays here — release/poll need it).
    fn ship(&mut self, frame: DecodedVkFrame) -> NativeVkFrame {
        let seq = self.next_seq;
        self.next_seq += 1;
        let token = NativeReleaseToken {
            seq,
            generation: frame.generation,
            presented: false,
        };
        let native = NativeVkFrame {
            image: frame.image.as_raw(),
            plane_views: [frame.plane_views[0].as_raw(), frame.plane_views[1].as_raw()],
            layer: frame.layer,
            layout: if frame.layout == vk::ImageLayout::VIDEO_DECODE_DPB_KHR {
                NativeVkLayout::DecodeDpb
            } else {
                NativeVkLayout::DecodeDst
            },
            semaphore: frame.semaphore.as_raw(),
            semaphore_value: frame.value,
            generation: frame.generation,
            width: frame.crop.width,
            height: frame.crop.height,
            coded_width: frame.coded_width,
            coded_height: frame.coded_height,
            crop_x: frame.crop.x,
            crop_y: frame.crop.y,
            // H.273 code points straight off the picture's ACTIVE SPS — per frame,
            // never latched, because the Windows host switches an HDR desktop to
            // PQ/BT.2020 IN-BAND (the Welcome still says SDR). pf-bitstream applies
            // E.2.1's "unspecified" inference (2/2/2, limited) where the VUI is
            // silent, and `csc_rows` resolves "unspecified" to its BT.709-limited
            // SDR default — same verdicts libavcodec's CICP passthrough produced.
            color: ColorDesc {
                primaries: frame.colour.colour_primaries,
                transfer: frame.colour.transfer_characteristics,
                matrix: frame.colour.matrix_coefficients,
                full_range: frame.colour.video_full_range,
            },
            keyframe: frame.is_idr,
            poc: frame.poc,
            guard: NativeReleaseGuard::new(
                self.release_tx
                    .as_ref()
                    .expect("release_tx lives until Drop")
                    .clone(),
                token,
            ),
        };
        self.outstanding.push(Shipped {
            seq,
            frame,
            released: false,
            presented: false,
            resolved: false,
            polls_after_release: 0,
        });
        native
    }

    /// Bounded wait for a shipped frame's decode-complete signal — the pump's
    /// sampled decode-latency stat (`Decoder::wait_hw_decoded`), one frame per
    /// stats window. The raw pair names a frame still in the shipped ledger (the
    /// pump waits on the same thread that just shipped it, before any settle
    /// could retire it); the ledger lookup is the liveness proof — an unreleased
    /// frame pins its pool, so a pair matching nothing (already settled, or a
    /// stray) just declines the sample instead of touching unknown handles.
    pub(crate) fn wait_timeline(&self, sem: u64, value: u64, timeout_ns: u64) -> bool {
        self.outstanding
            .iter()
            .find(|s| s.frame.semaphore.as_raw() == sem && s.frame.value == value)
            .is_some_and(|s| self.dec.wait_decoded(&s.frame, timeout_ns))
    }

    /// Drain the release channel, marking returned frames (release itself waits for
    /// the status read — see [`Self::settle_statuses`]).
    fn drain_releases(&mut self) {
        while let Ok(token) = self.release_rx.try_recv() {
            if !note_token(&mut self.outstanding, token) {
                tracing::debug!(
                    seq = token.seq,
                    generation = token.generation,
                    "release token without an outstanding frame"
                );
            }
        }
    }

    /// Poll the status query of every unresolved shipped frame (non-blocking) and
    /// release the ones that are both status-settled and token-returned. Returns how
    /// many frames NEWLY read `Failed` — driver-reported corruption.
    ///
    /// Polling an unreleased frame is always sound: its slot is pinned until
    /// `release_frame`, so the query slot it names cannot have been recycled under it
    /// (the false-`Failed` a recycled slot would read).
    fn settle_statuses(&mut self) -> u32 {
        let mut corrupt = 0u32;
        let Self {
            dec, outstanding, ..
        } = self;
        for s in outstanding.iter_mut() {
            if s.resolved {
                continue;
            }
            // A session rebuild (stream renegotiation) already made this frame stale:
            // its SESSION objects (query pool included) are gone — the picture pool
            // lives on in the decoder's graveyard while we hold the image, but the
            // query verdict is unknowable and poll_status would report the
            // conservative Failed — which is NOT driver corruption. Resolve it
            // quietly; the rebuild rode an IDR, so the stream has its re-anchor
            // already.
            if s.frame.generation != dec.generation() {
                tracing::debug!(
                    poc = s.frame.poc,
                    frame_generation = s.frame.generation,
                    "outstanding frame outlived its session generation — status unknowable"
                );
                s.resolved = true;
                continue;
            }
            match dec.poll_status(&s.frame) {
                DecodeStatus::Ok => s.resolved = true,
                DecodeStatus::Failed => {
                    s.resolved = true;
                    corrupt += 1;
                    tracing::warn!(
                        poc = s.frame.poc,
                        slot = s.frame.query_slot,
                        "decode status query: Failed (driver-reported corruption)"
                    );
                }
                DecodeStatus::Pending => {
                    if s.released {
                        // Token back ⇒ the decode op completed before the presenter's
                        // sampling ⇒ the query should be readable. Belt, not a path.
                        s.polls_after_release += 1;
                        if s.polls_after_release >= MAX_POLLS_AFTER_RELEASE {
                            tracing::debug!(
                                poc = s.frame.poc,
                                "status query still pending after release — giving \
                                 the slot back with an unknown verdict"
                            );
                            s.resolved = true;
                        }
                    }
                }
            }
        }
        outstanding.retain(|s| {
            if !(s.released && s.resolved) {
                return true;
            }
            match dec.release_frame(&s.frame, s.presented) {
                Ok(()) => {}
                // Not a best-effort no-op: stale-generation frames release into the
                // decoder's graveyard (a rebuild retires a still-held pool INTACT,
                // and this very call is what lets it die on its last token). An Err
                // is therefore a bookkeeping ghost — a double release — never a
                // held image left dangling.
                Err(e) => tracing::debug!(error = %e, "release_frame: {e}"),
            }
            false
        });
        corrupt
    }
}

impl Drop for NativeVulkanDecoder {
    fn drop(&mut self) {
        // Ordering contract: the run loop drops the PRESENTER's frame (its retired
        // slot, fence-waited) before joining the pump that owns this backend — so
        // by the time this Drop runs, outstanding tokens are either already in the
        // channel or arrive imminently; the bounded wait below is for that hand-off,
        // not for future GPU work.
        //
        // Frames never handed to the pump release directly (unsampled).
        for frame in std::mem::take(&mut self.deliverable) {
            if let Err(e) = self.dec.release_frame(&frame, false) {
                tracing::debug!(error = %e, "releasing an undelivered frame failed");
            }
        }
        // Drop our own sender FIRST: once every shipped guard is gone too, the
        // channel reports Disconnected — the "presenter can no longer produce
        // tokens" signal that short-circuits the wait instead of burning the full
        // budget against a presenter that is already gone.
        drop(self.release_tx.take());
        // Wait (bounded) for the presenter to hand back every shipped frame before
        // the decoder's Drop destroys the pool images: a returned token proves the
        // sampling submission's fence was waited, i.e. no GPU work of the
        // presenter's still reads the pools (the decoder's own drain covers only
        // decode work). Graveyarded pools ride the same token contract — a
        // mid-stream renegotiation retires a still-held pool INTACT, and the
        // release calls below route stale-generation frames into the graveyard,
        // so those pools too die only once their last presenter fence was waited.
        let deadline = Instant::now() + TEARDOWN_BUDGET;
        loop {
            self.drain_releases();
            let Self {
                dec, outstanding, ..
            } = self;
            outstanding.retain(|s| {
                if !s.released {
                    return true;
                }
                if let Err(e) = dec.release_frame(&s.frame, s.presented) {
                    tracing::debug!(error = %e, "teardown release_frame: {e}");
                }
                false
            });
            if self.outstanding.is_empty() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                tracing::warn!(
                    outstanding = self.outstanding.len(),
                    "native decode teardown: presenter still holds frames past the \
                     budget — destroying the pools anyway"
                );
                break;
            }
            match self
                .release_rx
                .recv_timeout((deadline - now).min(Duration::from_millis(50)))
            {
                Ok(token) => {
                    note_token(&mut self.outstanding, token);
                }
                // Every sender is gone (ours dropped above, every guard dropped):
                // no more tokens can EVER arrive — anything still outstanding is a
                // bookkeeping ghost, not a held frame. Stop waiting.
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if !self.outstanding.is_empty() {
                        tracing::debug!(
                            outstanding = self.outstanding.len(),
                            "release channel disconnected with entries outstanding — \
                             no tokens can arrive; proceeding with teardown"
                        );
                    }
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
            }
        }
        // `self.dec` drops after this body: it drains its own decode-side GPU work
        // and destroys any remaining graveyard pools (warned — a forfeit here means
        // the presenter kept frames past the budget).
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shipped-ledger entry with inert handles — the bookkeeping under test is pure.
    fn shipped(seq: u64, generation: u64) -> Shipped {
        Shipped {
            seq,
            frame: DecodedVkFrame {
                image: vk::Image::null(),
                view: vk::ImageView::null(),
                plane_views: [vk::ImageView::null(); 2],
                layer: 0,
                layout: vk::ImageLayout::VIDEO_DECODE_DST_KHR,
                coded_width: 1920,
                coded_height: 1088,
                crop: pf_bitstream_crop(1920, 1080),
                colour: pf_vkdecode::ColourDescription {
                    colour_primaries: 2,
                    transfer_characteristics: 2,
                    matrix_coefficients: 2,
                    video_full_range: false,
                },
                semaphore: vk::Semaphore::null(),
                value: 0,
                poc: 0,
                is_idr: false,
                query_slot: 0,
                submission: 0,
                picture: 0,
                generation,
            },
            released: false,
            presented: false,
            resolved: false,
            polls_after_release: 0,
        }
    }

    fn pf_bitstream_crop(width: u32, height: u32) -> pf_vkdecode::DisplayCrop {
        pf_vkdecode::DisplayCrop {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    #[test]
    fn release_tokens_mark_their_frame_and_tolerate_strays() {
        let mut outstanding = vec![shipped(0, 1), shipped(1, 1)];
        assert!(note_token(
            &mut outstanding,
            NativeReleaseToken {
                seq: 1,
                generation: 1,
                presented: true,
            }
        ));
        assert!(!outstanding[0].released);
        assert!(outstanding[1].released);
        assert!(
            outstanding[1].presented,
            "the token's presented flag rides into the ledger (the decoder waits \
             the presenter's value+1 write-back only when it was really enqueued)"
        );
        // A stray token (frame already settled away — e.g. a post-demotion drain)
        // matches nothing and must not panic or mis-mark.
        assert!(!note_token(
            &mut outstanding,
            NativeReleaseToken {
                seq: 7,
                generation: 1,
                presented: false,
            }
        ));
        assert!(!outstanding[0].released);
    }

    #[test]
    fn the_guard_sends_its_token_exactly_once_on_drop() {
        let (tx, rx) = mpsc::channel();
        let token = NativeReleaseToken {
            seq: 42,
            generation: 3,
            presented: false,
        };
        let guard = NativeReleaseGuard::new(tx, token);
        assert!(
            rx.try_recv().is_err(),
            "nothing is sent while the frame lives"
        );
        drop(guard);
        assert_eq!(rx.try_recv().ok(), Some(token), "drop sends the token");
        assert!(rx.try_recv().is_err(), "exactly once");
    }

    #[test]
    fn a_dropped_unpresented_frame_still_releases_through_the_same_guard() {
        // The newest-wins channel/store displacement path: the frame never reaches a
        // present, but dropping it must still return its slot.
        let (tx, rx) = mpsc::channel();
        let frame = NativeVkFrame {
            image: 0,
            plane_views: [0; 2],
            layer: 0,
            layout: NativeVkLayout::DecodeDst,
            semaphore: 0,
            semaphore_value: 0,
            generation: 5,
            width: 1920,
            height: 1080,
            coded_width: 1920,
            coded_height: 1088,
            crop_x: 0,
            crop_y: 0,
            color: ColorDesc {
                primaries: 2,
                transfer: 2,
                matrix: 2,
                full_range: false,
            },
            keyframe: true,
            poc: 0,
            guard: NativeReleaseGuard::new(
                tx,
                NativeReleaseToken {
                    seq: 9,
                    generation: 5,
                    presented: false,
                },
            ),
        };
        drop(frame);
        assert_eq!(
            rx.try_recv().ok(),
            Some(NativeReleaseToken {
                seq: 9,
                generation: 5,
                presented: false,
            }),
            "an unpresented drop reports presented=false — the decoder must not \
             wait a value+1 write-back that was never enqueued"
        );
    }

    #[test]
    fn a_dead_channel_is_ignored_not_fatal() {
        // Demotion mid-stream: the backend (and its Receiver) are gone while the
        // presenter still holds a frame — its drop must be a no-op, not a panic.
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let guard = NativeReleaseGuard::new(
            tx,
            NativeReleaseToken {
                seq: 1,
                generation: 1,
                presented: false,
            },
        );
        drop(guard); // must not panic
    }

    #[test]
    fn the_queue_lock_is_shared_only_when_the_families_collide() {
        // Same family ⇒ same VkQueue (both sides use index 0) ⇒ shared lock.
        assert!(submit_queues_collide(0, 0));
        assert!(submit_queues_collide(2, 2));
        // A separate decode family has exactly one submitter — no lock.
        assert!(!submit_queues_collide(0, 3));
    }
}
