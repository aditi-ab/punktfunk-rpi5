//! The winit application shell: one window hosting a Direct3D11 swapchain, the decoded-frame
//! present loop, and local keyboard/mouse capture forwarded on the wire contract.
//!
//! Input capture is a deliberate, reversible STATE (Moonlight-style, mirroring the other
//! native clients): engaged when the user clicks into the window (that click is suppressed
//! toward the host) or on first focus; released by Ctrl+Alt+Shift+Q (toggles) or focus loss —
//! held keys/buttons are flushed host-side on release so nothing sticks down. While captured
//! the cursor is hidden and confined; F11 toggles fullscreen.
//!
//! Keys are winit physical `KeyCode`s → VK via `keymap` (layout-independent). Mouse is
//! absolute (`MouseMoveAbs` scaled into the negotiated mode through the letterbox transform,
//! surface size packed in `flags`) — relative pointer-lock is a follow-up (RAWINPUT).

use crate::keymap;
use crate::present::{Renderer, SwapChain};
use crate::session::{SessionEvent, SessionHandle};
use crate::trust::{KnownHost, KnownHosts};
use crate::video::DecodedFrame;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::Mode;
use punktfunk_core::input::{InputEvent, InputKind};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use windows::Win32::Foundation::HWND;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

/// How we reached this host (for persisting a TOFU fingerprint after `Connected`).
pub struct ConnectInfo {
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// TOFU connect (no pin supplied) — persist the observed fingerprint on `Connected`.
    pub tofu: bool,
}

pub struct WinApp {
    handle: SessionHandle,
    info: ConnectInfo,
    /// App-lifetime SDL gamepad service: per-session capture + rumble/HID feedback.
    gamepad: crate::gamepad::GamepadService,

    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    swap: Option<SwapChain>,

    connector: Option<Arc<NativeClient>>,
    mode: Mode,
    have_frame: bool,

    captured: bool,
    modifiers: ModifiersState,
    held_keys: HashSet<u8>,
    held_buttons: HashSet<u32>,
}

impl WinApp {
    pub fn new(
        handle: SessionHandle,
        info: ConnectInfo,
        gamepad: crate::gamepad::GamepadService,
    ) -> WinApp {
        WinApp {
            handle,
            info,
            gamepad,
            window: None,
            renderer: None,
            swap: None,
            connector: None,
            mode: Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            have_frame: false,
            captured: false,
            modifiers: ModifiersState::empty(),
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
        }
    }

    pub fn run(self) -> anyhow::Result<()> {
        let event_loop = EventLoop::new()?;
        event_loop.set_control_flow(ControlFlow::Poll);
        let mut app = self;
        event_loop.run_app(&mut app)?;
        Ok(())
    }

    fn send(&self, kind: InputKind, code: u32, x: i32, y: i32, flags: u32) {
        if let Some(c) = &self.connector {
            let _ = c.send_input(&InputEvent {
                kind,
                _pad: [0; 3],
                code,
                x,
                y,
                flags,
            });
        }
    }

    /// Forward an absolute pointer position: window pixels → video pixels through the
    /// Contain-fit letterbox (`flags` packs the coordinate-space size, the host's contract).
    fn send_abs(&self, x: f64, y: f64) {
        let Some(window) = &self.window else { return };
        let size = window.inner_size();
        let (ww, wh) = (size.width.max(1) as f64, size.height.max(1) as f64);
        let (vw, vh) = (
            self.mode.width.max(1) as f64,
            self.mode.height.max(1) as f64,
        );
        let scale = (ww / vw).min(wh / vh);
        let (ox, oy) = ((ww - vw * scale) / 2.0, (wh - vh * scale) / 2.0);
        let px = (((x - ox) / scale).round()).clamp(0.0, vw - 1.0) as i32;
        let py = (((y - oy) / scale).round()).clamp(0.0, vh - 1.0) as i32;
        let flags = (self.mode.width << 16) | (self.mode.height & 0xffff);
        self.send(InputKind::MouseMoveAbs, 0, px, py, flags);
    }

    fn engage(&mut self) {
        if self.captured {
            return;
        }
        self.captured = true;
        if let Some(w) = &self.window {
            w.set_cursor_visible(false);
            // Confined keeps absolute mapping working; Locked (relative) is the follow-up.
            let _ = w.set_cursor_grab(CursorGrabMode::Confined);
        }
    }

    fn release(&mut self) {
        if !self.captured {
            return;
        }
        self.captured = false;
        if let Some(w) = &self.window {
            w.set_cursor_visible(true);
            let _ = w.set_cursor_grab(CursorGrabMode::None);
        }
        // Flush everything held so nothing sticks down on the host.
        for vk in self.held_keys.drain().collect::<Vec<_>>() {
            self.send(InputKind::KeyUp, vk as u32, 0, 0, 0);
        }
        for b in self.held_buttons.drain().collect::<Vec<_>>() {
            self.send(InputKind::MouseButtonUp, b, 0, 0, 0);
        }
    }

    fn toggle_fullscreen(&self) {
        if let Some(w) = &self.window {
            if w.fullscreen().is_some() {
                w.set_fullscreen(None);
            } else {
                w.set_fullscreen(Some(Fullscreen::Borderless(None)));
            }
        }
    }

    /// Drain session events + the newest decoded frame; returns true if a frame is ready to
    /// present. Called every loop turn.
    fn pump(&mut self, event_loop: &ActiveEventLoop) -> bool {
        while let Ok(ev) = self.handle.events.try_recv() {
            match ev {
                SessionEvent::Connected {
                    connector,
                    mode,
                    fingerprint,
                } => {
                    self.mode = mode;
                    if self.info.tofu {
                        let fp_hex = crate::trust::hex(&fingerprint);
                        let mut known = KnownHosts::load();
                        known.upsert(KnownHost {
                            name: self.info.name.clone(),
                            addr: self.info.addr.clone(),
                            port: self.info.port,
                            fp_hex: fp_hex.clone(),
                            paired: false,
                        });
                        let _ = known.save();
                        tracing::info!(fp = %fp_hex, "trusted on first use — pinned");
                    }
                    if let Some(w) = &self.window {
                        w.set_title(&format!(
                            "Punktfunk — {} · {}×{}@{}",
                            self.info.name, mode.width, mode.height, mode.refresh_hz
                        ));
                    }
                    self.gamepad.attach(connector.clone());
                    self.connector = Some(connector);
                    tracing::info!(?mode, "connected — streaming");
                }
                SessionEvent::Stats(s) => tracing::debug!(
                    fps = format!("{:.0}", s.fps),
                    mbps = format!("{:.1}", s.mbps),
                    lat_ms = format!("{:.2}", s.latency_ms),
                    "stats"
                ),
                SessionEvent::Failed {
                    msg,
                    trust_rejected,
                } => {
                    tracing::error!(%msg, trust_rejected, "connect failed");
                    if trust_rejected {
                        tracing::error!(
                            "host fingerprint changed or pairing required — re-pair with --pair PIN"
                        );
                    }
                    self.gamepad.detach();
                    event_loop.exit();
                    return false;
                }
                SessionEvent::Ended(err) => {
                    tracing::info!(reason = err.as_deref().unwrap_or("done"), "session ended");
                    self.gamepad.detach();
                    event_loop.exit();
                    return false;
                }
            }
        }
        // Keep only the newest frame (freshness over completeness).
        let mut newest = None;
        while let Ok(f) = self.handle.frames.try_recv() {
            newest = Some(f);
        }
        if let (Some(DecodedFrame::Cpu(c)), Some(r)) = (&newest, self.renderer.as_mut()) {
            if let Err(e) = r.upload(c) {
                tracing::warn!(error = %e, "frame upload failed");
            } else {
                self.have_frame = true;
            }
        }
        newest.is_some()
    }

    fn render(&mut self) {
        let (Some(swap), Some(renderer)) = (self.swap.as_mut(), self.renderer.as_ref()) else {
            return;
        };
        if !self.have_frame {
            return;
        }
        match swap.rtv() {
            Ok(rtv) => {
                renderer.draw(
                    &rtv,
                    swap.width,
                    swap.height,
                    self.mode.width,
                    self.mode.height,
                );
                swap.present();
            }
            Err(e) => tracing::warn!(error = %e, "acquire back buffer"),
        }
    }
}

fn hwnd_of(window: &Window) -> Option<HWND> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut core::ffi::c_void)),
        _ => None,
    }
}

/// winit MouseButton → GameStream button id (1=left, 2=middle, 3=right, 4=X1, 5=X2).
fn mouse_button_id(b: MouseButton) -> Option<u32> {
    Some(match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 4,
        MouseButton::Forward => 5,
        _ => return None,
    })
}

impl ApplicationHandler for WinApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Punktfunk")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!(error = %e, "create window");
                event_loop.exit();
                return;
            }
        };
        let renderer = match Renderer::new() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "D3D11 renderer");
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();
        let swap = hwnd_of(&window)
            .ok_or_else(|| anyhow::anyhow!("no HWND"))
            .and_then(|hwnd| {
                SwapChain::new(renderer.device(), hwnd, size.width, size.height)
                    .map_err(|e| anyhow::anyhow!(e))
            });
        match swap {
            Ok(s) => self.swap = Some(s),
            Err(e) => {
                tracing::error!(error = %e, "swapchain");
                event_loop.exit();
                return;
            }
        }
        self.renderer = Some(renderer);
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.handle.stop.store(true, Ordering::SeqCst);
                self.gamepad.detach();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(swap) = self.swap.as_mut() {
                    if let Err(e) = swap.resize(size.width, size.height) {
                        tracing::warn!(error = %e, "swapchain resize");
                    }
                }
            }
            WindowEvent::Focused(false) => self.release(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                // Local chords (intercepted, never forwarded): capture toggle + fullscreen.
                if code == KeyCode::KeyQ
                    && event.state.is_pressed()
                    && self.modifiers.control_key()
                    && self.modifiers.alt_key()
                    && self.modifiers.shift_key()
                {
                    if self.captured {
                        self.release();
                    } else {
                        self.engage();
                    }
                    return;
                }
                if code == KeyCode::F11 && event.state.is_pressed() {
                    self.toggle_fullscreen();
                    return;
                }
                if !self.captured {
                    return;
                }
                let Some(vk) = keymap::keycode_to_vk(code) else {
                    return;
                };
                if event.state.is_pressed() {
                    // Track held state for flush-on-release; re-send on auto-repeat too (the
                    // host treats KeyDown as a state set, so repeats are harmless).
                    self.held_keys.insert(vk);
                    self.send(InputKind::KeyDown, vk as u32, 0, 0, 0);
                } else if self.held_keys.remove(&vk) {
                    self.send(InputKind::KeyUp, vk as u32, 0, 0, 0);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.captured {
                    self.send_abs(position.x, position.y);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if !self.captured {
                    if state == ElementState::Pressed && button == MouseButton::Left {
                        self.engage(); // the engaging click is suppressed toward the host
                    }
                    return;
                }
                let Some(id) = mouse_button_id(button) else {
                    return;
                };
                if state == ElementState::Pressed {
                    self.held_buttons.insert(id);
                    self.send(InputKind::MouseButtonDown, id, 0, 0, 0);
                } else if self.held_buttons.remove(&id) {
                    self.send(InputKind::MouseButtonUp, id, 0, 0, 0);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if !self.captured {
                    return;
                }
                // The wire carries WHEEL_DELTA(120) units, positive = up / right.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / 120.0, p.y as f32 / 120.0),
                };
                let vy = (dy * 120.0) as i32;
                if vy != 0 {
                    self.send(InputKind::MouseScroll, 0, vy, 0, 0);
                }
                let vx = (dx * 120.0) as i32;
                if vx != 0 {
                    self.send(InputKind::MouseScroll, 1, vx, 0, 0);
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let new_frame = self.pump(event_loop);
        if new_frame {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        } else {
            // No frame this turn — yield briefly instead of spinning a core flat-out.
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
