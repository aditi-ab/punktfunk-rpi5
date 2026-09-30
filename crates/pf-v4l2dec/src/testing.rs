//! A decoder that follows the stateful contract without a device, for this
//! crate's tests and the client rung's.

use std::collections::VecDeque;
use std::time::Duration;

use crate::stateful::CaptureFormat;
use crate::stateful::Dequeued;
use crate::stateful::Device;
use crate::stateful::Event;
use crate::stateful::Interest;
use crate::stateful::Queue;

/// Decodes one picture per access unit, in order, copying its stamp. Nothing
/// decodes until the CAPTURE queue runs, and a source change pauses it again.
/// Panics on a buffer queued twice or out of range: a client bug, not a state.
#[derive(Default)]
pub struct FakeDecoder {
    /// Coded size the "stream" carries; a change raises a source change.
    pub stream: (u32, u32),
    /// The size last announced. `None` makes the next unit announce again.
    pub announced: Option<(u32, u32)>,
    /// The driver stopped consuming input.
    pub frozen: bool,
    /// CAPTURE allocations: `"allocate"` and `"free"`, in order.
    pub log: Vec<&'static str>,
    offered: Vec<u32>,
    capture_fourcc: u32,
    inputs: u32,
    /// Queued access units, oldest first.
    pending: VecDeque<(u32, u64)>,
    returned_inputs: VecDeque<u32>,
    capture_streaming: bool,
    capture_buffers: u32,
    queued_capture: VecDeque<u32>,
    done: VecDeque<Dequeued>,
    events: VecDeque<Event>,
}

impl FakeDecoder {
    pub fn new(width: u32, height: u32, offered: &[u32]) -> FakeDecoder {
        FakeDecoder {
            stream: (width, height),
            offered: offered.to_vec(),
            ..FakeDecoder::default()
        }
    }

    /// Resume a frozen decoder and let it work through its queue.
    pub fn thaw(&mut self) {
        self.frozen = false;
        self.run();
    }

    /// Decode whatever the contract allows right now.
    fn run(&mut self) {
        if self.frozen {
            return;
        }
        while let Some(&(index, stamp)) = self.pending.front() {
            if self.announced != Some(self.stream) {
                self.announced = Some(self.stream);
                self.capture_streaming = false;
                self.events.push_back(Event::SourceChange);
                return;
            }
            if !self.capture_streaming {
                return;
            }
            let Some(target) = self.queued_capture.pop_front() else {
                return;
            };
            self.pending.pop_front();
            self.returned_inputs.push_back(index);
            self.done.push_back(Dequeued {
                index: target,
                stamp,
                error: false,
                empty: false,
            });
        }
    }
}

impl Device for FakeDecoder {
    fn set_output_format(&mut self, _: u32, _: u32, _: u32, _: u32) -> std::io::Result<()> {
        Ok(())
    }

    fn subscribe_source_change(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn request_buffers(&mut self, queue: Queue, count: u32) -> std::io::Result<u32> {
        match queue {
            Queue::Output => self.inputs = count,
            Queue::Capture => {
                self.log.push(if count == 0 { "free" } else { "allocate" });
                self.capture_buffers = count;
                self.queued_capture.clear();
                self.done.clear();
            }
        }
        Ok(count)
    }

    fn stream(&mut self, queue: Queue, on: bool) -> std::io::Result<()> {
        if queue == Queue::Capture {
            self.capture_streaming = on;
            if !on {
                // STREAMOFF returns every buffer to the client, undecoded.
                self.queued_capture.clear();
                self.done.clear();
            }
            self.run();
        }
        Ok(())
    }

    fn queue_output(&mut self, index: u32, _: &[u8], stamp: u64) -> std::io::Result<()> {
        assert!(index < self.inputs, "input buffer {index} is not allocated");
        self.pending.push_back((index, stamp));
        self.run();
        Ok(())
    }

    fn dequeue_output(&mut self) -> std::io::Result<Option<u32>> {
        Ok(self.returned_inputs.pop_front())
    }

    fn queue_capture(&mut self, index: u32) -> std::io::Result<()> {
        assert!(
            index < self.capture_buffers,
            "buffer {index} is not in the pool"
        );
        assert!(
            !self.queued_capture.contains(&index),
            "buffer {index} queued twice"
        );
        self.queued_capture.push_back(index);
        self.run();
        Ok(())
    }

    fn dequeue_capture(&mut self) -> std::io::Result<Option<Dequeued>> {
        Ok(self.done.pop_front())
    }

    fn dequeue_event(&mut self) -> std::io::Result<Option<Event>> {
        Ok(self.events.pop_front())
    }

    fn capture_format(&mut self) -> std::io::Result<CaptureFormat> {
        let (width, height) = self.announced.unwrap_or(self.stream);
        Ok(CaptureFormat {
            fourcc: self.capture_fourcc,
            width,
            height,
            stride: width,
            planes: 1,
        })
    }

    fn capture_formats(&mut self) -> std::io::Result<Vec<u32>> {
        Ok(self.offered.clone())
    }

    fn set_capture_format(&mut self, fourcc: u32) -> std::io::Result<CaptureFormat> {
        self.capture_fourcc = fourcc;
        self.capture_format()
    }

    fn min_capture_buffers(&mut self) -> std::io::Result<u32> {
        Ok(2)
    }

    fn wait(&mut self, _: Interest, _: Duration) -> std::io::Result<()> {
        Ok(())
    }
}
