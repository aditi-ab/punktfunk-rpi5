//! V4L2 stateful decode: the hardware rung on SoCs whose decoder is a
//! memory-to-memory video node (Qualcomm `iris`/`venus`, MediaTek, Amlogic).
//! No ARM Mesa driver has Vulkan Video or VA-API, so this is their only rung.
//!
//! [`pf_v4l2dec::stateful`] runs the queue flow; this module is its ioctl
//! [`Device`], the node probe, and the hand-off. The driver parses the stream,
//! but the pump's facts — keyframe, colour, clean references, damage — still
//! come from the shared `pf-bitstream` planners, as on the CPU rung. Pictures
//! leave as [`DecodedImage::NativeDmabuf`]: each CAPTURE buffer is exported
//! once as a dma-buf and imported by the presenter as linear NV12 or P010.
//!
//! The driver's enumeration is the only truth: [`caps`] lists a codec only
//! where a node takes it and offers a linear picture format for it. Pin with
//! `PUNKTFUNK_DECODER=native-v4l2`; `PUNKTFUNK_V4L2_DEVICE` names the node.
//!
//! [`DecodedImage::NativeDmabuf`]: crate::video::DecodedImage::NativeDmabuf

use std::collections::VecDeque;
use std::os::fd::AsRawFd as _;
use std::os::fd::FromRawFd as _;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use pf_v4l2dec::stateful::fourcc_name;
use pf_v4l2dec::stateful::CaptureFormat;
use pf_v4l2dec::stateful::Dequeued;
use pf_v4l2dec::stateful::Device;
use pf_v4l2dec::stateful::Event;
use pf_v4l2dec::stateful::Interest;
use pf_v4l2dec::stateful::Picture;
use pf_v4l2dec::stateful::Queue;
use pf_v4l2dec::stateful::Stateful;
use pf_v4l2dec::uapi;

use crate::video::DecodeHealth;
use crate::video::DmabufFrame;
use crate::video::DmabufPlane;
use crate::video::DrmFrameGuard;
use crate::video::FrameGuard;
use crate::video::StreamFormat;
use crate::video::V4l2Summary;
use crate::video_color::ColorDesc;

/// `PUNKTFUNK_DECODER=native-v4l2`.
pub(crate) const DECODER_PIN: &str = "native-v4l2";

/// `DRM_FORMAT_MOD_LINEAR`. V4L2 buffers carry no modifier; the linear
/// formats this rung accepts are, by definition, this one.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Wait for a picture when none is in hand. A working decoder answers in a
/// few milliseconds; past this the pump moves on and takes it next call.
const WAIT_PICTURE: Duration = Duration::from_millis(30);

/// Extra wait for this access unit's own picture when an older one is in
/// hand. Short: it only exists to catch up after one late picture.
const WAIT_CATCH_UP: Duration = Duration::from_millis(5);

/// Access units outstanding with no picture before the decoder counts as
/// stalled. About a quarter second at 60 Hz.
const STALLED_AFTER: usize = 16;

/// What a node decodes, from its own format enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CodecNode {
    path: PathBuf,
    /// A linear 10-bit picture format is offered for this codec.
    ten_bit: bool,
}

/// V4L2 decode on this machine: per wire codec, the node that takes it.
#[derive(Debug, Default)]
pub(crate) struct Caps {
    h264: Option<CodecNode>,
    hevc: Option<CodecNode>,
    av1: Option<CodecNode>,
}

impl Caps {
    fn slot(&self, wire: u8) -> Option<&CodecNode> {
        match wire {
            punktfunk_core::quic::CODEC_H264 => self.h264.as_ref(),
            punktfunk_core::quic::CODEC_HEVC => self.hevc.as_ref(),
            punktfunk_core::quic::CODEC_AV1 => self.av1.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn summary(&self) -> V4l2Summary {
        let mut s = V4l2Summary::default();
        for wire in [
            punktfunk_core::quic::CODEC_H264,
            punktfunk_core::quic::CODEC_HEVC,
            punktfunk_core::quic::CODEC_AV1,
        ] {
            if let Some(node) = self.slot(wire) {
                s.codecs |= wire;
                if node.ten_bit {
                    s.ten_bit |= wire;
                }
            }
        }
        s
    }
}

/// The machine's V4L2 decoders, probed once. Opens each video node briefly.
pub(crate) fn caps() -> &'static Caps {
    static CAPS: OnceLock<Caps> = OnceLock::new();
    CAPS.get_or_init(probe)
}

fn wire_fourcc(wire: u8) -> Option<u32> {
    match wire {
        punktfunk_core::quic::CODEC_H264 => Some(uapi::V4L2_PIX_FMT_H264),
        punktfunk_core::quic::CODEC_HEVC => Some(uapi::V4L2_PIX_FMT_HEVC),
        punktfunk_core::quic::CODEC_AV1 => Some(uapi::V4L2_PIX_FMT_AV1),
        _ => None,
    }
}

fn probe() -> Caps {
    let mut caps = Caps::default();
    // The struct layouts in `pf_v4l2dec::uapi` are the 64-bit little-endian ABI.
    if !cfg!(all(target_pointer_width = "64", target_endian = "little")) {
        return caps;
    }
    for path in candidate_nodes() {
        let Ok(mut node) = Node::open(&path) else {
            continue;
        };
        let Ok(inputs) = node.formats(Queue::Output) else {
            continue;
        };
        for (wire, slot) in [
            (punktfunk_core::quic::CODEC_H264, &mut caps.h264),
            (punktfunk_core::quic::CODEC_HEVC, &mut caps.hevc),
            (punktfunk_core::quic::CODEC_AV1, &mut caps.av1),
        ] {
            let fourcc = wire_fourcc(wire).expect("the three listed codecs map");
            if slot.is_some() || !inputs.contains(&fourcc) {
                continue;
            }
            // Picture formats depend on the codec, so ask with it selected.
            if node.set_output_format(fourcc, 1920, 1080, 2 << 20).is_err() {
                continue;
            }
            let Ok(pictures) = node.formats(Queue::Capture) else {
                continue;
            };
            if !pictures.contains(&uapi::V4L2_PIX_FMT_NV12) {
                tracing::info!(
                    node = %path.display(),
                    codec = crate::video::wire_codec_name(wire),
                    offered = ?pictures.iter().map(|f| fourcc_name(*f)).collect::<Vec<_>>(),
                    "V4L2 decoder offers no linear NV12 for this codec — not used"
                );
                continue;
            }
            *slot = Some(CodecNode {
                path: path.clone(),
                ten_bit: pictures.contains(&uapi::V4L2_PIX_FMT_P010),
            });
        }
    }
    let s = caps.summary();
    if s.codecs != 0 {
        tracing::info!(
            h264 = ?caps.h264.as_ref().map(|n| n.path.display().to_string()),
            hevc = ?caps.hevc.as_ref().map(|n| n.path.display().to_string()),
            av1 = ?caps.av1.as_ref().map(|n| n.path.display().to_string()),
            ten_bit = format_args!("{:#04x}", s.ten_bit),
            "V4L2 stateful decoders found"
        );
    }
    caps
}

/// `PUNKTFUNK_V4L2_DEVICE`, else every `/dev/video*` in numeric order.
fn candidate_nodes() -> Vec<PathBuf> {
    if let Some(forced) = std::env::var_os("PUNKTFUNK_V4L2_DEVICE").filter(|v| !v.is_empty()) {
        return vec![PathBuf::from(forced)];
    }
    let Ok(dir) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut nodes: Vec<(u32, PathBuf)> = dir
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            let n = name.to_str()?.strip_prefix("video")?.parse().ok()?;
            Some((n, e.path()))
        })
        .collect();
    nodes.sort();
    nodes.into_iter().map(|(_, p)| p).collect()
}

/// One mapped OUTPUT buffer.
struct Mapping {
    ptr: std::ptr::NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is owned by exactly one `Node`, which is used from one
// thread at a time; nothing else holds the pointer.
unsafe impl Send for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are a live `mmap` result this value owns; it is
        // unmapped exactly once, here.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// What the rung needs of a decoder beyond the queue flow.
pub(crate) trait Opened: Device + Sized {
    fn open(path: &Path) -> std::io::Result<Self>;
    /// Export CAPTURE buffer `index` as a dma-buf.
    fn export(&mut self, index: u32) -> std::io::Result<OwnedFd>;
}

/// An open decoder node: the ioctl side of [`Device`].
pub(crate) struct Node {
    fd: OwnedFd,
    inputs: Vec<Mapping>,
}

/// `ioctl` with `EINTR` retried. `T` must be the struct `request` encodes.
fn ioctl<T>(fd: RawFd, request: u32, arg: &mut T) -> std::io::Result<()> {
    loop {
        // SAFETY: `fd` is an open video node and `arg` is a live, exclusively
        // borrowed `T` whose size is the one encoded in `request`, so the
        // kernel reads and writes only inside it.
        let r = unsafe { libc::ioctl(fd, request as _, std::ptr::from_mut(arg)) };
        if r >= 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

fn queue_type(queue: Queue) -> u32 {
    match queue {
        Queue::Output => uapi::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
        Queue::Capture => uapi::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
    }
}

/// `EAGAIN` and, on the CAPTURE queue after a drain, `EPIPE`: nothing to take.
fn nothing_ready(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EAGAIN | libc::EPIPE))
}

impl Node {
    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    fn formats(&mut self, queue: Queue) -> std::io::Result<Vec<u32>> {
        let mut out = Vec::new();
        for index in 0.. {
            let mut desc = uapi::V4l2Fmtdesc {
                index,
                type_: queue_type(queue),
                ..Default::default()
            };
            match ioctl(self.raw(), uapi::VIDIOC_ENUM_FMT, &mut desc) {
                Ok(()) => out.push(desc.pixelformat),
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    fn read_format(&mut self, queue: Queue) -> std::io::Result<uapi::V4l2Format> {
        let mut fmt = uapi::V4l2Format {
            type_: queue_type(queue),
            ..Default::default()
        };
        ioctl(self.raw(), uapi::VIDIOC_G_FMT, &mut fmt)?;
        Ok(fmt)
    }

    /// A `v4l2_buffer` for `queue` over `plane`, which must outlive the ioctl.
    fn buffer(queue: Queue, index: u32, plane: &mut uapi::V4l2Plane) -> uapi::V4l2Buffer {
        uapi::V4l2Buffer {
            index,
            type_: queue_type(queue),
            memory: uapi::V4L2_MEMORY_MMAP,
            m: std::ptr::from_mut(plane) as u64,
            length: 1,
            ..Default::default()
        }
    }

    /// Map OUTPUT buffer `index` for writing access units into.
    fn map_input(&mut self, index: u32) -> std::io::Result<Mapping> {
        let mut plane = uapi::V4l2Plane::default();
        let mut buf = Node::buffer(Queue::Output, index, &mut plane);
        ioctl(self.raw(), uapi::VIDIOC_QUERYBUF, &mut buf)?;
        let len = plane.length as usize;
        // SAFETY: a fresh shared mapping of this fd at the offset and length
        // the driver just reported for the buffer; no existing memory is
        // named, and the result is checked before use.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.raw(),
                (plane.m as u32).into(),
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let ptr = std::ptr::NonNull::new(ptr.cast::<u8>())
            .ok_or_else(|| std::io::Error::other("mmap returned null"))?;
        Ok(Mapping { ptr, len })
    }
}

impl Opened for Node {
    /// A multi-planar memory-to-memory streaming node, or a refusal.
    fn open(path: &Path) -> std::io::Result<Node> {
        let fd: OwnedFd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?
            .into();
        let mut cap = uapi::V4l2Capability::default();
        ioctl(fd.as_raw_fd(), uapi::VIDIOC_QUERYCAP, &mut cap)?;
        let device = if cap.capabilities & uapi::V4L2_CAP_DEVICE_CAPS != 0 {
            cap.device_caps
        } else {
            cap.capabilities
        };
        let needed = uapi::V4L2_CAP_VIDEO_M2M_MPLANE | uapi::V4L2_CAP_STREAMING;
        if device & needed != needed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "not a multi-planar memory-to-memory node",
            ));
        }
        Ok(Node {
            fd,
            inputs: Vec::new(),
        })
    }

    fn export(&mut self, index: u32) -> std::io::Result<OwnedFd> {
        let mut exp = uapi::V4l2Exportbuffer {
            type_: uapi::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            index,
            flags: (libc::O_CLOEXEC | libc::O_RDWR) as u32,
            ..Default::default()
        };
        ioctl(self.raw(), uapi::VIDIOC_EXPBUF, &mut exp)?;
        // SAFETY: a successful EXPBUF returns a new fd this process owns and
        // nothing else has seen.
        Ok(unsafe { OwnedFd::from_raw_fd(exp.fd) })
    }
}

fn capture_format_of(fmt: &uapi::V4l2Format) -> CaptureFormat {
    // Copied out: the mplane struct is packed.
    let pix = fmt.pix_mp;
    let planes = pix.plane_fmt;
    CaptureFormat {
        fourcc: pix.pixelformat,
        width: pix.width,
        height: pix.height,
        stride: planes[0].bytesperline,
        planes: pix.num_planes,
    }
}

impl Device for Node {
    fn set_output_format(
        &mut self,
        fourcc: u32,
        width: u32,
        height: u32,
        buffer_size: u32,
    ) -> std::io::Result<()> {
        let mut fmt = uapi::V4l2Format {
            type_: uapi::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            ..Default::default()
        };
        fmt.pix_mp.width = width;
        fmt.pix_mp.height = height;
        fmt.pix_mp.pixelformat = fourcc;
        fmt.pix_mp.field = uapi::V4L2_FIELD_NONE;
        fmt.pix_mp.num_planes = 1;
        fmt.pix_mp.plane_fmt[0].sizeimage = buffer_size;
        ioctl(self.raw(), uapi::VIDIOC_S_FMT, &mut fmt)
    }

    fn subscribe_source_change(&mut self) -> std::io::Result<()> {
        let mut sub = uapi::V4l2EventSubscription {
            type_: uapi::V4L2_EVENT_SOURCE_CHANGE,
            ..Default::default()
        };
        ioctl(self.raw(), uapi::VIDIOC_SUBSCRIBE_EVENT, &mut sub)
    }

    fn request_buffers(&mut self, queue: Queue, count: u32) -> std::io::Result<u32> {
        if queue == Queue::Output {
            self.inputs.clear();
        }
        let mut req = uapi::V4l2Requestbuffers {
            count,
            type_: queue_type(queue),
            memory: uapi::V4L2_MEMORY_MMAP,
            ..Default::default()
        };
        ioctl(self.raw(), uapi::VIDIOC_REQBUFS, &mut req)?;
        if queue == Queue::Output {
            for index in 0..req.count {
                let mapping = self.map_input(index)?;
                self.inputs.push(mapping);
            }
        }
        Ok(req.count)
    }

    fn stream(&mut self, queue: Queue, on: bool) -> std::io::Result<()> {
        let mut kind = queue_type(queue) as i32;
        let request = if on {
            uapi::VIDIOC_STREAMON
        } else {
            uapi::VIDIOC_STREAMOFF
        };
        ioctl(self.raw(), request, &mut kind)
    }

    fn queue_output(&mut self, index: u32, data: &[u8], stamp: u64) -> std::io::Result<()> {
        let mapping = self
            .inputs
            .get(index as usize)
            .ok_or_else(|| std::io::Error::other("no such input buffer"))?;
        if data.len() > mapping.len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "a {} byte access unit does not fit the {} byte input buffer",
                    data.len(),
                    mapping.len
                ),
            ));
        }
        // SAFETY: `mapping` is a live writable mapping of `mapping.len` bytes
        // and `data.len()` was just checked against it. The buffer is not
        // queued (the state machine only passes free indices), so the driver
        // is not reading it, and `data` cannot overlap a private mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), mapping.ptr.as_ptr(), data.len());
        }
        let mut plane = uapi::V4l2Plane {
            bytesused: data.len() as u32,
            ..Default::default()
        };
        let mut buf = Node::buffer(Queue::Output, index, &mut plane);
        buf.timestamp_sec = (stamp / 1_000_000) as i64;
        buf.timestamp_usec = (stamp % 1_000_000) as i64;
        ioctl(self.raw(), uapi::VIDIOC_QBUF, &mut buf)
    }

    fn dequeue_output(&mut self) -> std::io::Result<Option<u32>> {
        let mut plane = uapi::V4l2Plane::default();
        let mut buf = Node::buffer(Queue::Output, 0, &mut plane);
        match ioctl(self.raw(), uapi::VIDIOC_DQBUF, &mut buf) {
            Ok(()) => Ok(Some(buf.index)),
            Err(e) if nothing_ready(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn queue_capture(&mut self, index: u32) -> std::io::Result<()> {
        let mut plane = uapi::V4l2Plane::default();
        let mut buf = Node::buffer(Queue::Capture, index, &mut plane);
        ioctl(self.raw(), uapi::VIDIOC_QBUF, &mut buf)
    }

    fn dequeue_capture(&mut self) -> std::io::Result<Option<Dequeued>> {
        let mut plane = uapi::V4l2Plane::default();
        let mut buf = Node::buffer(Queue::Capture, 0, &mut plane);
        match ioctl(self.raw(), uapi::VIDIOC_DQBUF, &mut buf) {
            Ok(()) => Ok(Some(Dequeued {
                index: buf.index,
                stamp: (buf.timestamp_sec as u64) * 1_000_000 + buf.timestamp_usec as u64,
                error: buf.flags & uapi::V4L2_BUF_FLAG_ERROR != 0,
                empty: plane.bytesused == 0,
            })),
            Err(e) if nothing_ready(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn dequeue_event(&mut self) -> std::io::Result<Option<Event>> {
        let mut ev = uapi::V4l2Event::default();
        match ioctl(self.raw(), uapi::VIDIOC_DQEVENT, &mut ev) {
            Ok(()) => {
                let resolution = ev.u[0] as u32 & uapi::V4L2_EVENT_SRC_CH_RESOLUTION != 0;
                Ok(Some(
                    if ev.type_ == uapi::V4L2_EVENT_SOURCE_CHANGE && resolution {
                        Event::SourceChange
                    } else {
                        Event::Other
                    },
                ))
            }
            // No event pending.
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn capture_format(&mut self) -> std::io::Result<CaptureFormat> {
        Ok(capture_format_of(&self.read_format(Queue::Capture)?))
    }

    fn capture_formats(&mut self) -> std::io::Result<Vec<u32>> {
        self.formats(Queue::Capture)
    }

    fn set_capture_format(&mut self, fourcc: u32) -> std::io::Result<CaptureFormat> {
        let mut fmt = self.read_format(Queue::Capture)?;
        fmt.pix_mp.pixelformat = fourcc;
        ioctl(self.raw(), uapi::VIDIOC_S_FMT, &mut fmt)?;
        Ok(capture_format_of(&fmt))
    }

    fn min_capture_buffers(&mut self) -> std::io::Result<u32> {
        let mut ctrl = uapi::V4l2Control {
            id: uapi::V4L2_CID_MIN_BUFFERS_FOR_CAPTURE,
            value: 0,
        };
        ioctl(self.raw(), uapi::VIDIOC_G_CTRL, &mut ctrl)?;
        Ok(ctrl.value.max(0) as u32)
    }

    fn wait(&mut self, interest: Interest, timeout: Duration) -> std::io::Result<()> {
        let events = match interest {
            Interest::Capture => libc::POLLIN | libc::POLLPRI,
            Interest::Output => libc::POLLOUT | libc::POLLPRI,
        };
        let mut pfd = libc::pollfd {
            fd: self.raw(),
            events,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: `pfd` is one live `pollfd` and the count passed is 1.
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        // A queue that is not streaming polls as an error at once; without
        // this the caller's deadline loop would spin.
        if r > 0 && pfd.revents & events == 0 {
            std::thread::sleep(timeout.min(Duration::from_millis(2)));
        }
        Ok(())
    }
}

/// Which buffer a shipped frame gives back, and to which pool.
#[derive(Debug, Clone, Copy)]
struct Release {
    pool: u32,
    generation: u64,
    index: u32,
}

/// Holds one shipped picture's buffer until the presenter is done reading it.
/// The fds stay open past a pool rebuild so an imported frame never dangles.
pub struct V4l2FrameGuard {
    _fds: Arc<Vec<OwnedFd>>,
    tx: mpsc::Sender<Release>,
    release: Release,
}

impl Drop for V4l2FrameGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(self.release);
    }
}

/// Facts of the access unit a picture decodes, recorded when it was queued.
#[derive(Debug, Clone, Copy)]
struct Facts {
    keyframe: bool,
    references_clean: bool,
    color: ColorDesc,
    display: (u32, u32),
    /// The planner saw damage: decode it to keep the driver's references in
    /// step, but do not show it.
    damaged: bool,
}

/// What decides the input buffer size and the picture format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    coded: (u32, u32),
    bit_depth: u8,
}

enum Planner {
    H264(Box<pf_bitstream::h264::H264Planner>),
    H265(Box<pf_bitstream::h265::H265Planner>),
    Av1(Box<pf_bitstream::av1::Av1Planner>),
}

/// The planner's answer for one access unit.
enum Planned {
    /// Queue it; `Some` when a picture is expected back.
    Feed(Shape, Option<Facts>),
    /// Nothing decodable yet: wait for the keyframe.
    AwaitKeyframe,
    /// Dropped by rule (an HEVC RASL picture after a random access).
    Skip,
}

struct Session<D: Opened> {
    decoder: Stateful<D>,
    shape: Shape,
    /// This session's pools are `(pool, generation)`; `pool` is the session.
    pool: u32,
    /// dma-bufs of the current CAPTURE generation, by buffer index.
    exports: Arc<Vec<OwnedFd>>,
    exported_generation: u64,
    /// Queued access units still owed a picture, oldest first.
    pending: VecDeque<(u64, Facts)>,
}

pub(crate) struct NativeV4l2Decoder<D: Opened = Node> {
    wire: u8,
    node: PathBuf,
    planner: Planner,
    session: Option<Session<D>>,
    pools: u32,
    next_stamp: u64,
    health: DecodeHealth,
    recovery_request: bool,
    /// An access unit the planner counted never reached the driver, so the
    /// driver's references are wrong until the next keyframe.
    out_of_step: bool,
    release_tx: mpsc::Sender<Release>,
    release_rx: mpsc::Receiver<Release>,
}

fn colour_of(c: &pf_bitstream::h264::ColourDescription) -> ColorDesc {
    ColorDesc {
        primaries: c.colour_primaries,
        transfer: c.transfer_characteristics,
        matrix: c.matrix_coefficients,
        full_range: c.video_full_range,
    }
}

/// The visible size of a cropped picture. Planes are sampled from (0,0), so
/// a crop with another origin is refused rather than shown shifted.
fn display_of(crop: pf_bitstream::h264::DisplayCrop) -> Result<(u32, u32)> {
    if crop.x != 0 || crop.y != 0 {
        bail!(
            "conformance window at ({}, {}) — this rung hands the buffer over uncropped",
            crop.x,
            crop.y
        );
    }
    Ok((crop.width, crop.height))
}

impl NativeV4l2Decoder {
    /// Refuses when no node takes this codec and shape, so the ladder falls
    /// through at construction instead of on the first access unit.
    pub(crate) fn new(wire: u8, stream: StreamFormat) -> Result<NativeV4l2Decoder> {
        let node = caps()
            .slot(wire)
            .ok_or_else(|| anyhow!("no V4L2 decoder node takes this codec"))?;
        if stream.chroma_format_idc != punktfunk_core::quic::CHROMA_IDC_420 {
            bail!("V4L2 decode is 4:2:0 only");
        }
        if stream.bit_depth > 8 && !node.ten_bit {
            bail!("the V4L2 decoder offers no linear 10-bit picture format");
        }
        Ok(NativeV4l2Decoder::on_node(wire, node.path.clone()))
    }
}

impl<D: Opened> NativeV4l2Decoder<D> {
    /// The rung over the decoder at `node`, opened on the first access unit.
    fn on_node(wire: u8, node: PathBuf) -> NativeV4l2Decoder<D> {
        let planner = match wire {
            punktfunk_core::quic::CODEC_H264 => {
                Planner::H264(Box::new(pf_bitstream::h264::H264Planner::new()))
            }
            punktfunk_core::quic::CODEC_HEVC => {
                Planner::H265(Box::new(pf_bitstream::h265::H265Planner::new()))
            }
            _ => Planner::Av1(Box::new(pf_bitstream::av1::Av1Planner::new())),
        };
        let (release_tx, release_rx) = mpsc::channel();
        NativeV4l2Decoder {
            wire,
            node,
            planner,
            session: None,
            pools: 0,
            next_stamp: 0,
            health: DecodeHealth {
                // The driver has no per-picture status query this rung reads.
                status_queries: false,
                ..DecodeHealth::default()
            },
            recovery_request: false,
            out_of_step: false,
            release_tx,
            release_rx,
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        match self.planner {
            Planner::H264(_) => "native-v4l2 h264",
            Planner::H265(_) => "native-v4l2 h265",
            Planner::Av1(_) => "native-v4l2 av1",
        }
    }

    pub(crate) fn health(&self) -> DecodeHealth {
        self.health
    }

    pub(crate) fn take_recovery_request(&mut self) -> bool {
        std::mem::take(&mut self.recovery_request)
    }

    pub(crate) fn forgive_unclean(&mut self) {
        match &mut self.planner {
            Planner::H264(p) => p.forgive_unclean(),
            Planner::H265(p) => p.forgive_unclean(),
            Planner::Av1(p) => p.forgive_unclean(),
        }
    }

    /// One access unit in, at most one picture out. `Ok(None)` is the decoder
    /// still working, a withheld damaged picture, or the wait for a keyframe.
    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<DmabufFrame>> {
        self.drain_releases();
        let result = self.decode_inner(au);
        match &result {
            Ok(Some((_, damaged))) => self.health.note(*damaged, false, 0),
            Ok(None) => {}
            Err(_) => self.health.note(false, true, 0),
        }
        Ok(result?.and_then(|(frame, _)| frame))
    }

    fn decode_inner(&mut self, au: &[u8]) -> Result<Option<(Option<DmabufFrame>, bool)>> {
        let (shape, mut facts) = match self.plan(au)? {
            Planned::Feed(shape, facts) => (shape, facts),
            Planned::AwaitKeyframe => {
                self.recovery_request = true;
                return Ok(None);
            }
            Planned::Skip => return Ok(None),
        };
        let keyframe = facts.is_some_and(|f| f.keyframe);
        if let Some(f) = facts.as_mut() {
            f.damaged |= self.out_of_step && !keyframe;
        }
        let damaged = facts.is_some_and(|f| f.damaged);
        if damaged {
            self.recovery_request = true;
        }
        let stamp = self.next_stamp;
        self.next_stamp += 1;
        let queued = self
            .ensure_session(shape)
            .and_then(|s| s.decoder.submit(au, stamp).map_err(|e| anyhow!("{e}")));
        if let Err(e) = queued {
            // The planner has moved past this unit; the driver never saw it.
            self.out_of_step = true;
            return Err(e);
        }
        if keyframe {
            self.out_of_step = false;
        }
        let s = self.session.as_mut().expect("just queued into it");
        if let Some(facts) = facts {
            s.pending.push_back((stamp, facts));
        }
        if s.pending.len() > STALLED_AFTER {
            let owed = s.pending.len();
            // Start over at the next keyframe instead of failing every unit.
            s.pending.clear();
            bail!("the V4L2 decoder returned no picture for {owed} access units");
        }
        let Some(picture) = self.newest_picture(stamp)? else {
            return Ok(Some((None, damaged)));
        };
        let frame = self.ship(picture)?;
        Ok(Some((frame, damaged)))
    }

    /// Picture facts for `au` from the shared planner. The driver decodes on
    /// its own parse; this only tells the pump what the picture is.
    fn plan(&mut self, au: &[u8]) -> Result<Planned> {
        match &mut self.planner {
            Planner::H264(p) => {
                let plan = match p.plan_au(au) {
                    Ok(plan) => plan,
                    Err(e) if e.awaits_idr() => return Ok(Planned::AwaitKeyframe),
                    Err(e) => return Err(anyhow!("{e:?}")),
                };
                let pic = &plan.picture;
                Ok(Planned::Feed(
                    Shape {
                        coded: (pic.coded_width, pic.coded_height),
                        bit_depth: 8 + pic.bit_depth_luma_minus8,
                    },
                    Some(Facts {
                        keyframe: pic.is_idr,
                        references_clean: pic.references_clean,
                        color: colour_of(&pic.colour),
                        display: display_of(pic.display_crop)?,
                        damaged: plan
                            .warnings
                            .iter()
                            .any(pf_bitstream::h264::PlanWarning::is_integrity),
                    }),
                ))
            }
            Planner::H265(p) => {
                let plan = match p.plan_au(au) {
                    Ok(plan) => plan,
                    Err(pf_bitstream::h265::PlanError::RaslSkipped { .. }) => {
                        return Ok(Planned::Skip)
                    }
                    Err(e) if e.awaits_idr() => return Ok(Planned::AwaitKeyframe),
                    Err(e) => return Err(anyhow!("{e:?}")),
                };
                let pic = &plan.picture;
                Ok(Planned::Feed(
                    Shape {
                        coded: (pic.coded_width, pic.coded_height),
                        bit_depth: 8 + pic.bit_depth_luma_minus8,
                    },
                    Some(Facts {
                        keyframe: pic.is_idr,
                        references_clean: pic.references_clean,
                        color: colour_of(&pic.colour),
                        display: display_of(pic.display_crop)?,
                        damaged: plan
                            .warnings
                            .iter()
                            .any(pf_bitstream::h265::PlanWarning::is_integrity),
                    }),
                ))
            }
            Planner::Av1(p) => {
                let plans = p.plan_au(au).map_err(|e| anyhow!("{e}"))?;
                let Some(first) = plans.first() else {
                    return Ok(Planned::Skip);
                };
                let shape = Shape {
                    coded: (
                        u32::from(first.sequence.max_frame_width_minus_1) + 1,
                        u32::from(first.sequence.max_frame_height_minus_1) + 1,
                    ),
                    bit_depth: first.picture.bit_depth,
                };
                let damaged = plans.iter().any(|plan| {
                    plan.warnings
                        .iter()
                        .any(pf_bitstream::av1::PlanWarning::is_integrity)
                });
                // A temporal unit shows at most one frame; hidden ones are
                // references the driver keeps to itself.
                let shown = plans.iter().rev().find(|plan| !plan.dpb.outputs.is_empty());
                let facts = shown.map(|plan| Facts {
                    keyframe: plan.picture.is_key,
                    references_clean: plan.picture.references_clean,
                    color: colour_of(&plan.picture.colour),
                    display: (
                        plan.picture.render_width.min(plan.picture.upscaled_width),
                        plan.picture.render_height.min(plan.picture.frame_height),
                    ),
                    damaged,
                });
                Ok(Planned::Feed(shape, facts))
            }
        }
    }

    /// A new stream shape gets a new node session: the input buffers are
    /// sized for it and the picture format follows its bit depth.
    fn ensure_session(&mut self, shape: Shape) -> Result<&mut Session<D>> {
        if self.session.as_ref().is_some_and(|s| s.shape == shape) {
            return Ok(self.session.as_mut().expect("just matched"));
        }
        if let Some(old) = self.session.take() {
            tracing::info!(from = ?old.shape, to = ?shape,
                "V4L2 stream renegotiated — reopening the decoder");
        }
        let wanted = if shape.bit_depth > 8 {
            uapi::V4L2_PIX_FMT_P010
        } else {
            uapi::V4L2_PIX_FMT_NV12
        };
        let fourcc = wire_fourcc(self.wire).expect("construction checked the codec");
        let node = D::open(&self.node).with_context(|| format!("open {}", self.node.display()))?;
        let decoder = Stateful::open(node, fourcc, shape.coded.0, shape.coded.1, &[wanted])
            .map_err(|e| anyhow!("{e}"))
            .context("start the V4L2 decoder")?;
        self.pools += 1;
        Ok(self.session.insert(Session {
            decoder,
            shape,
            pool: self.pools,
            exports: Arc::new(Vec::new()),
            exported_generation: 0,
            pending: VecDeque::new(),
        }))
    }

    fn drain_releases(&mut self) {
        while let Ok(r) = self.release_rx.try_recv() {
            let Some(s) = self.session.as_mut().filter(|s| s.pool == r.pool) else {
                continue;
            };
            if let Err(e) = s.decoder.release(r.index, r.generation) {
                tracing::debug!(error = %e, "V4L2: a picture buffer did not requeue");
            }
        }
    }

    /// The newest ready picture. Waits for one, then briefly for `stamp`'s
    /// own; an older picture it overtakes goes straight back to the decoder.
    fn newest_picture(&mut self, stamp: u64) -> Result<Option<Picture>> {
        let s = self.session.as_mut().expect("decode built the session");
        let mut deadline = Instant::now() + WAIT_PICTURE;
        let mut best: Option<Picture> = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(p) = s.decoder.pump(left).map_err(|e| anyhow!("{e}"))? else {
                break;
            };
            if let Some(older) = best.replace(p) {
                s.decoder
                    .release(older.index, older.generation)
                    .map_err(|e| anyhow!("{e}"))?;
                if self.health.note_dropped() {
                    tracing::warn!(
                        dropped_total = self.health.dropped,
                        "native V4L2: the decoder is running behind — dropping the older picture"
                    );
                }
            }
            if p.stamp >= stamp {
                break;
            }
            deadline = deadline.min(Instant::now() + WAIT_CATCH_UP);
        }
        Ok(best)
    }

    /// Build the presenter's frame, or give the buffer back when the picture
    /// must not be shown.
    fn ship(&mut self, picture: Picture) -> Result<Option<DmabufFrame>> {
        let s = self.session.as_mut().expect("decode built the session");
        // Drivers copy the input stamp to its picture. Older entries are
        // units the decoder produced nothing for.
        while s.pending.front().is_some_and(|(st, _)| *st < picture.stamp) {
            s.pending.pop_front();
        }
        // No entry: a unit the stall reset forgot, or one that shows nothing.
        let facts = match s.pending.front() {
            Some((st, _)) if *st == picture.stamp => s.pending.pop_front().map(|(_, f)| f),
            _ => None,
        };
        let give_back = |s: &mut Session<D>| {
            s.decoder
                .release(picture.index, picture.generation)
                .map_err(|e| anyhow!("{e}"))
        };
        let Some(facts) = facts else {
            give_back(s)?;
            return Ok(None);
        };
        if facts.damaged || picture.corrupt {
            self.recovery_request = true;
            give_back(s)?;
            return Ok(None);
        }
        let capture = s
            .decoder
            .capture()
            .expect("a picture implies a configured queue");
        let format = capture.format;
        let buffers = capture.buffers;
        if s.exported_generation != picture.generation {
            let mut fds = Vec::with_capacity(buffers as usize);
            for index in 0..buffers {
                fds.push(
                    s.decoder
                        .device()
                        .export(index)
                        .context("export a V4L2 picture buffer")?,
                );
            }
            s.exports = Arc::new(fds);
            s.exported_generation = picture.generation;
            tracing::info!(
                format = %fourcc_name(format.fourcc),
                coded = format_args!("{}x{}", format.width, format.height),
                stride = format.stride,
                buffers,
                "native V4L2 picture pool ready"
            );
        }
        let fd = s.exports[picture.index as usize].as_raw_fd();
        // One memory plane: chroma follows luma at the coded height.
        let chroma_offset = format
            .stride
            .checked_mul(format.height)
            .ok_or_else(|| anyhow!("V4L2 picture geometry overflows"))?;
        Ok(Some(DmabufFrame {
            width: facts.display.0,
            height: facts.display.1,
            coded_width: format.width,
            coded_height: format.height,
            // The V4L2 codes for NV12 and P010 are the DRM ones.
            fourcc: format.fourcc,
            modifier: DRM_FORMAT_MOD_LINEAR,
            planes: vec![
                DmabufPlane {
                    fd,
                    offset: 0,
                    stride: format.stride,
                },
                DmabufPlane {
                    fd,
                    offset: chroma_offset,
                    stride: format.stride,
                },
            ],
            color: facts.color,
            keyframe: facts.keyframe,
            references_clean: facts.references_clean,
            // A dequeued buffer is finished: there is no fence to wait.
            sync_fds: Vec::new(),
            // Pool and generation both name a distinct set of buffers.
            pool_key: (u64::from(s.pool) << 48)
                | ((picture.generation & 0xffff) << 32)
                | u64::from(picture.index),
            path: DECODER_PIN,
            guard: DrmFrameGuard(FrameGuard::V4l2(V4l2FrameGuard {
                _fds: s.exports.clone(),
                tx: self.release_tx.clone(),
                release: Release {
                    pool: s.pool,
                    generation: picture.generation,
                    index: picture.index,
                },
            })),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_bitstream::testing::split_h264_aus;
    use pf_bitstream::testing::H264_25FPS;
    use pf_v4l2dec::testing::FakeDecoder;

    impl Opened for FakeDecoder {
        fn open(_: &Path) -> std::io::Result<FakeDecoder> {
            // Larger than the vendored stream: the rung takes the coded size
            // from the decoder and the visible one from the bitstream.
            Ok(FakeDecoder::new(384, 256, &[uapi::V4L2_PIX_FMT_NV12]))
        }

        fn export(&mut self, _: u32) -> std::io::Result<OwnedFd> {
            Ok(std::fs::File::open("/dev/null")?.into())
        }
    }

    fn rung() -> NativeV4l2Decoder<FakeDecoder> {
        NativeV4l2Decoder::on_node(punktfunk_core::quic::CODEC_H264, PathBuf::from("fake"))
    }

    fn fake(d: &mut NativeV4l2Decoder<FakeDecoder>) -> &mut FakeDecoder {
        d.session.as_mut().expect("a session").decoder.device()
    }

    /// The whole path short of a device: planner facts, queueing, the stamp
    /// pairing, the dma-buf layout, and the guard that returns each buffer.
    #[test]
    fn the_vendored_stream_decodes_one_picture_per_unit() {
        let mut d = rung();
        let aus = split_h264_aus(H264_25FPS);
        assert!(aus.len() > 8, "more units than the pool has buffers");
        let mut keys = Vec::new();
        for (n, au) in aus.iter().enumerate() {
            let f = d
                .decode(au)
                .expect("decode")
                .unwrap_or_else(|| panic!("unit {n} produced no picture"));
            assert_eq!(f.path, "native-v4l2");
            assert_eq!((f.coded_width, f.coded_height), (384, 256));
            assert!(f.width <= f.coded_width && f.height <= f.coded_height);
            assert_eq!(f.fourcc, uapi::V4L2_PIX_FMT_NV12);
            assert_eq!(f.modifier, DRM_FORMAT_MOD_LINEAR);
            assert_eq!(f.planes.len(), 2);
            assert_eq!((f.planes[0].offset, f.planes[0].stride), (0, 384));
            assert_eq!(
                f.planes[1].offset,
                384 * 256,
                "chroma follows the coded luma"
            );
            assert!(f.sync_fds.is_empty());
            assert!(f.keyframe || n > 0, "the stream opens on an IDR");
            keys.push(f.pool_key);
        }
        // Dropping each frame returned its buffer: six buffers carried them all.
        keys.sort_unstable();
        keys.dedup();
        assert!(keys.len() <= 6, "{} distinct buffers", keys.len());
        assert_eq!(fake(&mut d).log, ["allocate"]);
        assert!(!d.take_recovery_request());
    }

    /// A picture the presenter still holds is never decoded into, and a full
    /// pool waits instead of failing.
    #[test]
    fn held_frames_keep_their_buffers_until_dropped() {
        let mut d = rung();
        let aus = split_h264_aus(H264_25FPS);
        let mut held = Vec::new();
        let mut next = aus.iter();
        while held.len() < 6 {
            let f = d
                .decode(next.next().unwrap())
                .unwrap()
                .expect("a free buffer");
            assert!(held.iter().all(|h: &DmabufFrame| h.pool_key != f.pool_key));
            held.push(f);
        }
        assert!(d.decode(next.next().unwrap()).unwrap().is_none());
        held.remove(0);
        assert!(d.decode(next.next().unwrap()).unwrap().is_some());
    }

    /// A unit the driver never received breaks its reference chain: nothing
    /// is shown, and a keyframe is asked for, until one arrives.
    #[test]
    fn a_lost_unit_withholds_pictures_until_the_next_keyframe() {
        let mut d = rung();
        let aus = split_h264_aus(H264_25FPS);
        assert!(d.decode(aus[0]).unwrap().is_some());
        fake(&mut d).frozen = true;
        let lost = aus[1..]
            .iter()
            .position(|au| d.decode(au).is_err())
            .expect("a frozen decoder runs out of input buffers");
        fake(&mut d).thaw();
        let after = aus[lost + 2];
        assert!(d.decode(after).unwrap().is_none(), "references are wrong");
        assert!(d.take_recovery_request());
        // The host answers with an IDR; the stream's first unit is one.
        let f = d.decode(aus[0]).unwrap().expect("the keyframe shows");
        assert!(f.keyframe);
        assert!(!d.take_recovery_request());
    }
}
