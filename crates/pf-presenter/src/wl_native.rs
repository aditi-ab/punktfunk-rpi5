//! Native Wayland lane: a decoded picture's dma-buf becomes the window's own buffer.
//!
//! The Vulkan presenter draws every picture through a colour-conversion pass into a
//! swapchain the compositor then composites. Here a dma-buf goes straight on SDL's
//! `wl_surface` through `zwp_linux_dmabuf_v1`, so the compositor can put it on a plane, and
//! `wp_presentation` stamps the glass for the HUD. The buffer is a VAAPI surface as decoded,
//! or a copy of a Vulkan Video picture (`vk::export_ring`). The lane takes a picture only
//! when the surface feedback lists its format and modifier, it is SDR, and it fills the
//! window; anything else is declined and the Vulkan path draws that frame. The presenter
//! suspends its swapchain while the lane owns the window: Mesa's explicit-sync object on the
//! surface would make a plain dma-buf commit a fatal protocol error.
//!
//! Each buffer is imported once under a key and reused; the caller's hold is kept until the
//! compositor releases the buffer. The overlay rides on its own subsurface above the picture,
//! with an empty input region. Opt-in (`PUNKTFUNK_NATIVE_SCANOUT=1`) until measured on a display.
//!
//! SDL owns the socket. A private queue takes this lane's events; SDL's pump reads them in
//! and [`NativeLane::pump`] dispatches them. Presentation times arrive on CLOCK_MONOTONIC
//! and are moved onto the session's realtime clock as they land.

use anyhow::{Context as _, Result};
use pf_client_core::video::{ColorDesc, DmabufFrame};
use punktfunk_core::video_fit::VideoFit;
use sdl3::video::WindowContext;
use std::any::Any;
use std::collections::HashMap;
use std::os::fd::BorrowedFd;
use std::sync::Arc;
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_registry, wl_subcompositor, wl_subsurface, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::color_representation::v1::client::{
    wp_color_representation_manager_v1 as crm, wp_color_representation_surface_v1 as crs,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_feedback_v1 as feedback,
    zwp_linux_dmabuf_v1 as dmabuf,
};
use wayland_protocols::wp::presentation_time::client::{
    wp_presentation, wp_presentation_feedback as pfb,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const CLOCK_MONOTONIC: u32 = 1;
/// `wp_presentation_feedback.kind` bit: the buffer reached the screen without a copy.
const KIND_ZERO_COPY: u32 = 8;
/// linux-dmabuf tranche flag: this tranche's buffers can go straight to a plane.
const TRANCHE_SCANOUT: u32 = 1;

/// `PUNKTFUNK_NATIVE_SCANOUT=1` arms the lane. Off until the overlay rides along.
pub fn enabled() -> bool {
    pf_client_core::video::native_scanout_wanted()
}

/// What the lane did with a VAAPI frame.
pub enum Outcome {
    Shown,
    /// The lane owns the window but this frame's buffer is busy: the frame is skipped.
    Dropped,
    /// Not this lane's frame (or not yet): the caller draws it through Vulkan.
    Declined(DmabufFrame),
}

/// Where a keyed buffer stands with the compositor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotState {
    /// Never offered.
    Unknown,
    Pending,
    /// Imported and not on screen: a commit may use it.
    Free,
    /// The compositor may still read it.
    Held,
    Failed,
}

/// One presented frame's stamps, client realtime clock.
pub struct NativeSample {
    pub pts_ns: u64,
    pub decoded_ns: u64,
    pub submitted_ns: u64,
    pub displayed_ns: u64,
    pub zero_copy: bool,
}

struct Slot {
    buffer: wl_buffer::WlBuffer,
    /// The caller's hold (a decoder slot, a ring slot), kept while the compositor may read.
    held: Option<Box<dyn Any>>,
}

enum Import {
    Pending(params::ZwpLinuxBufferParamsV1),
    Ready(Slot),
    Failed,
}

struct Job {
    pts_ns: u64,
    decoded_ns: u64,
    submitted_ns: u64,
}

#[derive(Default)]
struct LaneState {
    clock_id: Option<u32>,
    table: Vec<(u32, u64)>,
    /// Every (fourcc, modifier) pair the surface's tranches list.
    pairs: Vec<(u32, u64)>,
    /// The pairs a scanout tranche lists.
    scanout: Vec<(u32, u64)>,
    tranche_flags: u32,
    feedback_done: bool,
    /// Bumped on every complete feedback: a caller that chose a modifier re-chooses.
    feedback_gen: u64,
    /// Buffer params in flight, by protocol id, to the key they import.
    pending: HashMap<u32, u64>,
    imports: HashMap<u64, Import>,
    by_buffer: HashMap<ObjectId, u64>,
    jobs: HashMap<usize, Job>,
    samples: Vec<NativeSample>,
    zero_copy: u32,
    presented: u32,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for LaneState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_presentation::WpPresentation, ()> for LaneState {
    fn event(
        state: &mut Self,
        _: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.clock_id = Some(clk_id);
        }
    }
}

impl Dispatch<pfb::WpPresentationFeedback, usize> for LaneState {
    fn event(
        state: &mut Self,
        _: &pfb::WpPresentationFeedback,
        event: pfb::Event,
        seq: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            pfb::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                flags,
                ..
            } => {
                let Some(job) = state.jobs.remove(seq) else {
                    return;
                };
                let sec = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                let mono = sec * 1_000_000_000 + u64::from(tv_nsec);
                let kind = match flags {
                    WEnum::Value(k) => k.bits(),
                    WEnum::Unknown(v) => v,
                };
                let zero_copy = kind & KIND_ZERO_COPY != 0;
                state.presented += 1;
                state.zero_copy += u32::from(zero_copy);
                state.samples.push(NativeSample {
                    pts_ns: job.pts_ns,
                    decoded_ns: job.decoded_ns,
                    submitted_ns: job.submitted_ns,
                    displayed_ns: monotonic_to_realtime(mono),
                    zero_copy,
                });
            }
            pfb::Event::Discarded => {
                state.jobs.remove(seq);
            }
            _ => {}
        }
    }
}

impl Dispatch<feedback::ZwpLinuxDmabufFeedbackV1, ()> for LaneState {
    fn event(
        state: &mut Self,
        _: &feedback::ZwpLinuxDmabufFeedbackV1,
        event: feedback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            feedback::Event::FormatTable { fd, size } => {
                use std::os::unix::fs::FileExt as _;
                let file = std::fs::File::from(fd);
                let mut bytes = vec![0u8; size as usize];
                if file.read_exact_at(&mut bytes, 0).is_ok() {
                    state.table = bytes
                        .chunks_exact(16)
                        .map(|c| {
                            let f = u32::from_ne_bytes([c[0], c[1], c[2], c[3]]);
                            let m = u64::from_ne_bytes([
                                c[8], c[9], c[10], c[11], c[12], c[13], c[14], c[15],
                            ]);
                            (f, m)
                        })
                        .collect();
                }
                // A new table starts a new answer.
                state.pairs.clear();
                state.scanout.clear();
                state.feedback_done = false;
            }
            feedback::Event::TrancheFlags { flags } => {
                state.tranche_flags = match flags {
                    WEnum::Value(f) => f.bits(),
                    WEnum::Unknown(v) => v,
                };
            }
            feedback::Event::TrancheFormats { indices } => {
                for c in indices.chunks_exact(2) {
                    let i = u16::from_ne_bytes([c[0], c[1]]) as usize;
                    if let Some(&pair) = state.table.get(i) {
                        if !state.pairs.contains(&pair) {
                            state.pairs.push(pair);
                        }
                        if state.tranche_flags & TRANCHE_SCANOUT != 0
                            && !state.scanout.contains(&pair)
                        {
                            state.scanout.push(pair);
                        }
                    }
                }
            }
            feedback::Event::TrancheDone => state.tranche_flags = 0,
            feedback::Event::Done => {
                state.feedback_done = true;
                state.feedback_gen += 1;
            }
            _ => {}
        }
    }
}

impl Dispatch<params::ZwpLinuxBufferParamsV1, ()> for LaneState {
    fn event(
        state: &mut Self,
        prm: &params::ZwpLinuxBufferParamsV1,
        event: params::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(key) = state.pending.remove(&prm.id().protocol_id()) else {
            return;
        };
        match event {
            params::Event::Created { buffer } => {
                state.by_buffer.insert(buffer.id(), key);
                state
                    .imports
                    .insert(key, Import::Ready(Slot { buffer, held: None }));
            }
            params::Event::Failed => {
                state.imports.insert(key, Import::Failed);
            }
            _ => {}
        }
        prm.destroy();
    }

    wayland_client::event_created_child!(LaneState, params::ZwpLinuxBufferParamsV1, [
        params::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, ()),
    ]);
}

impl Dispatch<wl_buffer::WlBuffer, ()> for LaneState {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            if let Some(key) = state.by_buffer.get(&buffer.id()) {
                if let Some(Import::Ready(slot)) = state.imports.get_mut(key) {
                    slot.held = None;
                }
            }
        }
    }
}

delegate_noop!(LaneState: ignore dmabuf::ZwpLinuxDmabufV1);
delegate_noop!(LaneState: ignore crm::WpColorRepresentationManagerV1);
delegate_noop!(LaneState: ignore crs::WpColorRepresentationSurfaceV1);
delegate_noop!(LaneState: ignore wl_compositor::WlCompositor);
delegate_noop!(LaneState: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(LaneState: ignore wl_subsurface::WlSubsurface);
delegate_noop!(LaneState: ignore wl_region::WlRegion);
delegate_noop!(LaneState: ignore wl_surface::WlSurface);
delegate_noop!(LaneState: ignore wp_viewporter::WpViewporter);
delegate_noop!(LaneState: ignore wp_viewport::WpViewport);

/// The overlay's own surface above the picture: input passes through to SDL's surface.
struct Hud {
    surface: wl_surface::WlSurface,
    sub: wl_subsurface::WlSubsurface,
    viewport: wp_viewport::WpViewport,
    mapped: bool,
}

/// What the overlay's surface needs from the compositor.
struct HudGlobals {
    compositor: wl_compositor::WlCompositor,
    subcompositor: wl_subcompositor::WlSubcompositor,
    viewporter: wp_viewporter::WpViewporter,
}

fn monotonic_to_realtime(mono_ns: u64) -> u64 {
    let now_mono = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let now_mono = now_mono.tv_sec as u64 * 1_000_000_000 + now_mono.tv_nsec as u64;
    let now_real = pf_client_core::session::now_ns();
    now_real.wrapping_add(mono_ns).wrapping_sub(now_mono)
}

pub struct NativeLane {
    conn: Connection,
    queue: EventQueue<LaneState>,
    qh: QueueHandle<LaneState>,
    state: LaneState,
    globals: Option<GlobalList>,
    surface: wl_surface::WlSurface,
    dmabuf: dmabuf::ZwpLinuxDmabufV1,
    presentation: wp_presentation::WpPresentation,
    color_repr: Option<crm::WpColorRepresentationManagerV1>,
    repr: Option<crs::WpColorRepresentationSurfaceV1>,
    /// (matrix, full range) last told to the compositor.
    repr_set: Option<(u8, bool)>,
    /// SDL scales the buffer to the window through its viewport; without one the buffer must
    /// match the window.
    has_viewport: bool,
    /// `None` where the compositor lacks a subcompositor or viewporter: no overlay surface.
    hud_globals: Option<HudGlobals>,
    hud: Option<Hud>,
    seq: usize,
    dead: bool,
    // SAFETY: field drop order keeps SDL's display alive past every borrowed proxy and queue.
    _window: Arc<WindowContext>,
}

impl NativeLane {
    /// The lane on SDL's Wayland connection, or `None` where the compositor lacks dma-buf
    /// feedback or presentation timing (or the window is not Wayland).
    pub fn new(window: &sdl3::video::Window) -> Result<Option<Self>> {
        let driver = window.subsystem().current_video_driver();
        if driver != "wayland" {
            tracing::info!(
                driver,
                "native scanout: SDL is not on Wayland — Vulkan presents"
            );
            return Ok(None);
        }
        // SAFETY: the live window owns the pointers; none transfers ownership.
        let (display, surface_ptr, viewport_ptr) = unsafe {
            let props = sdl3::sys::video::SDL_GetWindowProperties(window.raw());
            let get = |name| {
                sdl3::sys::properties::SDL_GetPointerProperty(props, name, std::ptr::null_mut())
            };
            (
                get(sdl3::sys::video::SDL_PROP_WINDOW_WAYLAND_DISPLAY_POINTER),
                get(sdl3::sys::video::SDL_PROP_WINDOW_WAYLAND_SURFACE_POINTER),
                get(sdl3::sys::video::SDL_PROP_WINDOW_WAYLAND_VIEWPORT_POINTER),
            )
        };
        if display.is_null() || surface_ptr.is_null() {
            return Ok(None);
        }
        // SAFETY: window.context() is retained until after the foreign backend is dropped.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, mut queue) =
            registry_queue_init::<LaneState>(&conn).context("native lane registry")?;
        let qh = queue.handle();
        let dmabuf: dmabuf::ZwpLinuxDmabufV1 = match globals.bind(&qh, 4..=5, ()) {
            Ok(d) => d,
            Err(e) => {
                tracing::info!(error = %e, "native scanout: no linux-dmabuf v4 — Vulkan presents");
                return Ok(None);
            }
        };
        let presentation: wp_presentation::WpPresentation = match globals.bind(&qh, 1..=2, ()) {
            Ok(p) => p,
            Err(e) => {
                tracing::info!(error = %e, "native scanout: no wp_presentation — Vulkan presents");
                return Ok(None);
            }
        };
        let color_repr: Option<crm::WpColorRepresentationManagerV1> =
            globals.bind(&qh, 1..=1, ()).ok();
        let hud_globals = match (
            globals.bind::<wl_compositor::WlCompositor, _, _>(&qh, 4..=6, ()),
            globals.bind::<wl_subcompositor::WlSubcompositor, _, _>(&qh, 1..=1, ()),
            globals.bind::<wp_viewporter::WpViewporter, _, _>(&qh, 1..=1, ()),
        ) {
            (Ok(compositor), Ok(subcompositor), Ok(viewporter)) => Some(HudGlobals {
                compositor,
                subcompositor,
                viewporter,
            }),
            _ => None,
        };
        // SAFETY: SDL's live wl_surface proxy on this display; the interface matches.
        let surface_id =
            unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface_ptr.cast()) }
                .context("SDL wl_surface id")?;
        let surface =
            wl_surface::WlSurface::from_id(&conn, surface_id).context("SDL wl_surface proxy")?;
        dmabuf.get_surface_feedback(&surface, &qh, ());
        let mut state = LaneState::default();
        for _ in 0..4 {
            queue
                .roundtrip(&mut state)
                .context("native lane feedback")?;
            if state.feedback_done && state.clock_id.is_some() {
                break;
            }
        }
        if state.clock_id != Some(CLOCK_MONOTONIC) {
            tracing::info!(
                clock = ?state.clock_id,
                "native scanout: presentation clock is not CLOCK_MONOTONIC — Vulkan presents"
            );
            return Ok(None);
        }
        tracing::info!(
            pairs = state.pairs.len(),
            scanout_pairs = state.scanout.len(),
            color_representation = color_repr.is_some(),
            viewport = !viewport_ptr.is_null(),
            overlay_surface = hud_globals.is_some(),
            "native scanout lane armed on SDL's surface"
        );
        Ok(Some(Self {
            conn,
            queue,
            qh,
            state,
            globals: Some(globals),
            surface,
            dmabuf,
            presentation,
            color_repr,
            repr: None,
            repr_set: None,
            has_viewport: !viewport_ptr.is_null(),
            hud_globals,
            hud: None,
            seq: 0,
            dead: false,
            _window: window.context(),
        }))
    }

    /// The compositor can take the overlay on its own surface above the picture.
    pub fn overlay_supported(&self) -> bool {
        self.hud_globals.is_some() && !self.dead
    }

    /// Show the free buffer under `key` as the overlay, scaled to the window's `logical`
    /// size, and keep `hold` until the compositor releases it.
    pub fn overlay_show(&mut self, key: u64, logical: (u32, u32), hold: Box<dyn Any>) -> bool {
        if self.dead || self.slot_state(key) != SlotState::Free {
            return false;
        }
        let Some(g) = &self.hud_globals else {
            return false;
        };
        let hud = self.hud.get_or_insert_with(|| {
            let surface = g.compositor.create_surface(&self.qh, ());
            let sub = g
                .subcompositor
                .get_subsurface(&surface, &self.surface, &self.qh, ());
            sub.set_position(0, 0);
            sub.set_desync();
            // An empty input region: the pointer stays on SDL's surface underneath.
            let region = g.compositor.create_region(&self.qh, ());
            surface.set_input_region(Some(&region));
            region.destroy();
            let viewport = g.viewporter.get_viewport(&surface, &self.qh, ());
            Hud {
                surface,
                sub,
                viewport,
                mapped: false,
            }
        });
        let Some(Import::Ready(slot)) = self.state.imports.get_mut(&key) else {
            return false;
        };
        hud.viewport
            .set_destination(logical.0.max(1) as i32, logical.1.max(1) as i32);
        hud.surface.attach(Some(&slot.buffer), 0, 0);
        hud.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        hud.surface.commit();
        hud.mapped = true;
        slot.held = Some(hold);
        self.flush();
        true
    }

    /// Unmap the overlay's surface; the compositor releases its buffer.
    pub fn overlay_hide(&mut self) {
        let Some(hud) = self.hud.as_mut().filter(|h| h.mapped) else {
            return;
        };
        hud.surface.attach(None, 0, 0);
        hud.surface.commit();
        hud.mapped = false;
        self.flush();
    }

    /// Dispatch what SDL's socket reads brought for this lane. A protocol error retires the
    /// lane; the connection itself is SDL's to fail on.
    pub fn pump(&mut self) {
        if self.dead {
            return;
        }
        if self.queue.dispatch_pending(&mut self.state).is_err()
            || self.conn.protocol_error().is_some()
        {
            tracing::warn!("native scanout: Wayland error — the lane retires, Vulkan presents");
            self.dead = true;
        }
    }

    fn flush(&mut self) {
        if let Err(wayland_backend::client::WaylandError::Protocol(_)) = self.conn.flush() {
            self.dead = true;
        }
    }

    /// Whether a picture of this size and colour can be the window's buffer: SDR, and it
    /// fills the window (through SDL's viewport, or at the window's own size without one).
    pub fn fits(
        &self,
        color: ColorDesc,
        frame: (u32, u32),
        view: (u32, u32),
        fit: VideoFit,
    ) -> bool {
        if self.dead || color.is_pq() {
            return false;
        }
        let (vw, vh) = (u64::from(view.0), u64::from(view.1));
        let (fw, fh) = (u64::from(frame.0), u64::from(frame.1));
        if vw == 0 || vh == 0 || fw == 0 || fh == 0 {
            return false;
        }
        if !self.has_viewport {
            return (fw, fh) == (vw, vh);
        }
        // Stretch fills by definition; fit and crop only when no bars or cut would show.
        fit == VideoFit::Stretch || (vw * fh).abs_diff(vh * fw) * 100 <= vw * fh
    }

    /// The surface feedback lists this (fourcc, modifier).
    pub fn lists(&self, fourcc: u32, modifier: u64) -> bool {
        modifier != DRM_FORMAT_MOD_INVALID && self.state.pairs.contains(&(fourcc, modifier))
    }

    /// Modifiers the surface takes for `fourcc`, scanout tranches first.
    pub fn modifiers_for(&self, fourcc: u32) -> Vec<u64> {
        let scanout = self.state.scanout.iter().filter(|(f, _)| *f == fourcc);
        let rest = self.state.pairs.iter().filter(|(f, _)| *f == fourcc);
        let mut out: Vec<u64> = Vec::new();
        for &(_, m) in scanout.chain(rest) {
            if m != DRM_FORMAT_MOD_INVALID && !out.contains(&m) {
                out.push(m);
            }
        }
        out
    }

    /// Changes whenever the compositor sends new surface feedback.
    pub fn feedback_generation(&self) -> u64 {
        self.state.feedback_gen
    }

    pub fn is_dead(&self) -> bool {
        self.dead
    }

    pub fn slot_state(&self, key: u64) -> SlotState {
        match self.state.imports.get(&key) {
            None => SlotState::Unknown,
            Some(Import::Pending(_)) => SlotState::Pending,
            Some(Import::Failed) => SlotState::Failed,
            Some(Import::Ready(slot)) if slot.held.is_some() => SlotState::Held,
            Some(Import::Ready(_)) => SlotState::Free,
        }
    }

    /// Offer a dma-buf under `key`: one `(fd, offset, pitch)` per memory plane. The answer
    /// arrives with a later [`Self::pump`]; libwayland dups each fd while marshalling.
    pub fn import(
        &mut self,
        key: u64,
        size: (u32, u32),
        fourcc: u32,
        modifier: u64,
        planes: &[(BorrowedFd<'_>, u32, u32)],
    ) {
        let prm = self.dmabuf.create_params(&self.qh, ());
        for (i, (fd, offset, pitch)) in planes.iter().enumerate() {
            prm.add(
                *fd,
                i as u32,
                *offset,
                *pitch,
                (modifier >> 32) as u32,
                modifier as u32,
            );
        }
        prm.create(size.0 as i32, size.1 as i32, fourcc, params::Flags::empty());
        self.state.pending.insert(prm.id().protocol_id(), key);
        self.state.imports.insert(key, Import::Pending(prm));
        self.flush();
    }

    /// Drop the buffer under `key`. The compositor keeps its own reference to a buffer it
    /// still shows; the hold goes now.
    pub fn forget(&mut self, key: u64) {
        match self.state.imports.remove(&key) {
            Some(Import::Ready(slot)) => {
                self.state.by_buffer.remove(&slot.buffer.id());
                slot.buffer.destroy();
            }
            Some(Import::Pending(prm)) => {
                self.state.pending.remove(&prm.id().protocol_id());
                prm.destroy();
            }
            _ => {}
        }
        self.flush();
    }

    fn apply_color(&mut self, color: ColorDesc) {
        let Some(manager) = &self.color_repr else {
            return;
        };
        let want = (color.matrix, color.full_range);
        if self.repr_set == Some(want) {
            return;
        }
        let coefficients = match color.matrix {
            5 | 6 => crs::Coefficients::Bt601,
            9 | 10 => crs::Coefficients::Bt2020,
            _ => crs::Coefficients::Bt709,
        };
        let range = if color.full_range {
            crs::Range::Full
        } else {
            crs::Range::Limited
        };
        let repr = self
            .repr
            .get_or_insert_with(|| manager.get_surface(&self.surface, &self.qh, ()));
        repr.set_coefficients_and_range(coefficients, range);
        self.repr_set = Some(want);
    }

    /// Put the free buffer under `key` on the window and keep `hold` until the compositor
    /// releases it. `false` when the buffer is not free; `hold` is dropped then.
    pub fn commit(
        &mut self,
        key: u64,
        color: ColorDesc,
        hold: Box<dyn Any>,
        pts_ns: u64,
        decoded_ns: u64,
    ) -> bool {
        if self.dead || self.slot_state(key) != SlotState::Free {
            return false;
        }
        self.apply_color(color);
        let seq = self.seq;
        self.seq += 1;
        let Some(Import::Ready(slot)) = self.state.imports.get_mut(&key) else {
            return false;
        };
        self.surface.attach(Some(&slot.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.presentation.feedback(&self.surface, &self.qh, seq);
        self.state.jobs.insert(
            seq,
            Job {
                pts_ns,
                decoded_ns,
                submitted_ns: pf_client_core::session::now_ns(),
            },
        );
        self.surface.commit();
        slot.held = Some(hold);
        self.flush();
        true
    }

    /// A refused import of a listed pair retires the lane for the session.
    pub fn refused(&mut self, fourcc: u32, modifier: u64) {
        tracing::warn!(
            fourcc = format!("{fourcc:#010x}"),
            modifier = format!("{modifier:#x}"),
            "native scanout: the compositor refused a listed dma-buf — the lane retires"
        );
        self.dead = true;
    }

    /// Whether a VAAPI frame can be the window's buffer: listed pair and it [`Self::fits`].
    pub fn takes(&self, d: &DmabufFrame, view: (u32, u32), fit: VideoFit) -> bool {
        self.lists(d.fourcc, d.modifier) && self.fits(d.color, (d.width, d.height), view, fit)
    }

    /// Where a VAAPI frame's pool slot stands, importing it on first sight: in the
    /// background, or with `now` on the spot (one roundtrip). `Failed` also when the lane is
    /// dead.
    pub fn prepare(&mut self, d: &DmabufFrame, now: bool) -> SlotState {
        self.pump();
        if self.dead {
            return SlotState::Failed;
        }
        if self.slot_state(d.pool_key) == SlotState::Unknown {
            let planes: Vec<_> = d
                .planes
                .iter()
                // SAFETY: the frame owns each fd for this call; libwayland dups it.
                .map(|p| (unsafe { BorrowedFd::borrow_raw(p.fd) }, p.offset, p.stride))
                .collect();
            self.import(
                d.pool_key,
                (d.width, d.height),
                d.fourcc,
                d.modifier,
                &planes,
            );
            if now && self.queue.roundtrip(&mut self.state).is_err() {
                self.dead = true;
                return SlotState::Failed;
            }
        }
        self.slot_state(d.pool_key)
    }

    /// Commit a VAAPI frame whose slot [`Self::prepare`] reported free; the frame's guard
    /// stays with the buffer until the compositor releases it.
    pub fn commit_vaapi(&mut self, d: DmabufFrame, pts_ns: u64, decoded_ns: u64) -> bool {
        // The pump waited the decode already; a leftover sync_file costs a poll.
        for fd in &d.sync_fds {
            use std::os::fd::AsRawFd as _;
            let _ = pf_zerocopy::dmabuf_fence::wait_sync_file(fd.as_raw_fd(), 50);
        }
        let (key, color) = (d.pool_key, d.color);
        let DmabufFrame { guard, .. } = d;
        self.commit(key, color, Box::new(guard), pts_ns, decoded_ns)
    }

    /// Frames the compositor reported on glass since the last call.
    pub fn take_samples(&mut self) -> Vec<NativeSample> {
        self.pump();
        std::mem::take(&mut self.state.samples)
    }

    /// (zero-copy, presented) since the last call.
    pub fn take_zero_copy(&mut self) -> (u32, u32) {
        let out = (self.state.zero_copy, self.state.presented);
        self.state.zero_copy = 0;
        self.state.presented = 0;
        out
    }
}

impl Drop for NativeLane {
    fn drop(&mut self) {
        if let Some(hud) = self.hud.take() {
            hud.viewport.destroy();
            hud.sub.destroy();
            hud.surface.destroy();
        }
        if let Some(g) = self.hud_globals.take() {
            g.subcompositor.destroy();
            g.viewporter.destroy();
        }
        for import in self.state.imports.values() {
            match import {
                Import::Ready(slot) => slot.buffer.destroy(),
                Import::Pending(prm) => prm.destroy(),
                Import::Failed => {}
            }
        }
        if let Some(repr) = self.repr.take() {
            repr.destroy();
        }
        self.dmabuf.destroy();
        self.presentation.destroy();
        if let Some(globals) = self.globals.take() {
            let id = globals.registry().id();
            globals.destroy();
            let _ = self.conn.backend().destroy_object(&id);
        }
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    /// The lane's realtime stamp moves with the wall clock, never with the monotonic offset
    /// alone: a presented time "now" lands at "now" on the session clock.
    #[test]
    fn a_presented_now_lands_on_the_session_clock_now() {
        let mono = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let mono = mono.tv_sec as u64 * 1_000_000_000 + mono.tv_nsec as u64;
        let real = pf_client_core::session::now_ns();
        let got = super::monotonic_to_realtime(mono);
        assert!(got.abs_diff(real) < 50_000_000, "{got} vs {real}");
    }
}
