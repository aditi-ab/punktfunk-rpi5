//! Playback vitals: what the device callback knows and the decode thread logs.
//!
//! The PipeWire callback (`audio.rs`) runs on the graph's realtime data loop now
//! (`RT_PROCESS`), where formatting a log line — a `String` allocation, the subscriber's mutex,
//! a write to stderr and the log ring — is exactly the class of thing a realtime thread must
//! not do: at best it is a priority inversion against whatever holds the lock, at worst it is a
//! missed graph cycle, which is a click. So the callback publishes numbers into these atomics
//! and the decode thread, an ordinary thread that already wakes every frame, prints them at the
//! old cadence with the old field names (`audio playback buffer_ms= target_ms= underruns=
//! drift_sheds= plc_ms=`), so a field-log grep keeps working. The WASAPI twin runs its render
//! loop on a plain thread and could log in place, but publishes here too: one logging site,
//! one line shape, on both platforms.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Counters and gauges the device callback publishes. All `Relaxed`: every field is a
/// self-contained reading, nothing here orders anything else.
#[derive(Debug, Default)]
pub struct PlaybackVitals {
    /// Device callbacks served (primed or not) — proof of life for the pull side.
    pub callbacks: AtomicU64,
    /// Callbacks the ring could not fill (a genuine underrun) — see `JitterPolicy::note_read`.
    pub underruns: AtomicU64,
    /// Drops the policy asked for: drift sheds and hard trims together.
    pub sheds: AtomicU64,
    /// The policy's smoothed ring depth, ms — what drift correction reacts to.
    pub buffer_ms: AtomicU32,
    /// The policy's LIVE target depth, ms (grows under underrun pressure, follows A/V sync).
    pub target_ms: AtomicU32,
    /// The device quantum as first seen: frames the graph/engine asked for per callback, the
    /// mapped buffer's capacity, and what we actually write. `write_frames == 0` = not seen yet.
    pub requested_frames: AtomicU32,
    pub capacity_frames: AtomicU32,
    pub write_frames: AtomicU32,
}

impl PlaybackVitals {
    /// Callback side: one callback done. `ran_short` = it could not be filled from the ring;
    /// `shed` = the policy dropped something this callback.
    pub fn note_callback(&self, ran_short: bool, shed: bool, buffer_ms: u32, target_ms: u32) {
        self.callbacks.fetch_add(1, Ordering::Relaxed);
        if ran_short {
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
        if shed {
            self.sheds.fetch_add(1, Ordering::Relaxed);
        }
        self.buffer_ms.store(buffer_ms, Ordering::Relaxed);
        self.target_ms.store(target_ms, Ordering::Relaxed);
    }

    /// Callback side: the quantum, published once (the first callback that has one).
    pub fn note_quantum(&self, requested: u32, capacity: u32, write: u32) {
        self.requested_frames.store(requested, Ordering::Relaxed);
        self.capacity_frames.store(capacity, Ordering::Relaxed);
        self.write_frames.store(write, Ordering::Relaxed);
    }

    /// Whether [`note_quantum`](Self::note_quantum) has been called.
    pub fn quantum_known(&self) -> bool {
        self.write_frames.load(Ordering::Relaxed) > 0
    }

    /// A consistent-enough snapshot for a log line (each field is read once; a callback landing
    /// between two reads skews a counter by one, which a 10 s log line does not care about).
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            callbacks: self.callbacks.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            sheds: self.sheds.load(Ordering::Relaxed),
            buffer_ms: self.buffer_ms.load(Ordering::Relaxed),
            target_ms: self.target_ms.load(Ordering::Relaxed),
            requested_frames: self.requested_frames.load(Ordering::Relaxed),
            capacity_frames: self.capacity_frames.load(Ordering::Relaxed),
            write_frames: self.write_frames.load(Ordering::Relaxed),
        }
    }
}

/// One reading of [`PlaybackVitals`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub callbacks: u64,
    pub underruns: u64,
    pub sheds: u64,
    pub buffer_ms: u32,
    pub target_ms: u32,
    pub requested_frames: u32,
    pub capacity_frames: u32,
    pub write_frames: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_gauges_overwrite() {
        let v = PlaybackVitals::default();
        assert!(!v.quantum_known());
        v.note_callback(false, false, 15, 15);
        v.note_callback(true, true, 9, 25);
        v.note_callback(true, false, 12, 25);
        v.note_quantum(240, 8192, 240);
        let s = v.snapshot();
        assert_eq!(s.callbacks, 3);
        assert_eq!(s.underruns, 2);
        assert_eq!(s.sheds, 1);
        assert_eq!(
            (s.buffer_ms, s.target_ms),
            (12, 25),
            "gauges hold the latest reading"
        );
        assert_eq!(
            (s.requested_frames, s.capacity_frames, s.write_frames),
            (240, 8192, 240)
        );
        assert!(v.quantum_known());
    }
}
