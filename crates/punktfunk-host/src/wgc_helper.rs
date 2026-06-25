//! USER-session WGC helper (Windows) — part of the two-process secure-desktop design
//! (docs/windows-secure-desktop.md).
//!
//! WGC won't activate under the SYSTEM account, but the host must run as SYSTEM for the secure
//! desktop. So the SYSTEM host spawns THIS helper in the interactive user session
//! (`CreateProcessAsUserW`) to do the WGC capture + NVENC encode that needs the user token, and the
//! helper ships the encoded Annex-B access units back over its **stdout** pipe (which the host
//! inherits + reads). The host relays them on the live QUIC session while the normal desktop is up,
//! and switches to its own DDA encoder on the secure desktop. The helper captures the SAME SudoVDA
//! output **by GDI name only** — it never creates a virtual output / touches display topology (a
//! second topology owner would re-trigger the ACCESS_LOST born-lost storm).
//!
//! Wire framing on stdout, per AU: `[u32 len LE][u64 pts_ns LE][u8 keyframe][len bytes data]`.

use crate::capture::{dxgi::WinCaptureTarget, wgc::WgcCapturer, Capturer};
use crate::encode::{self, Codec};
use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct HelperOptions {
    pub target_id: u32,
    pub gdi_name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Negotiated encode bit depth (8, or 10 = HEVC Main10). HDR auto-upgrades to 10 from the
    /// captured frame's `Rgb10a2` format regardless.
    pub bit_depth: u8,
}

/// AU framing magic + version, so the host can resync / detect a helper crash on its stdout stream.
const AU_MAGIC: u32 = 0x5046_4155; // "PFAU"

/// Control byte the host writes on our stdin to force the next frame to be an IDR. Must match
/// `wgc_relay::CTL_KEYFRAME`.
const CTL_KEYFRAME: u8 = 0x01;

pub fn run(opts: HelperOptions) -> Result<()> {
    tracing::info!(
        target_id = opts.target_id,
        gdi = %opts.gdi_name,
        mode = format!("{}x{}@{}", opts.width, opts.height, opts.fps),
        "WGC helper starting (user session)"
    );

    // This thread does WGC capture + video-processor convert + NVENC submit — the GPU-submitting hot
    // path. Elevate its OS priority so a CPU-heavy game can't deschedule it and delay submission (which
    // would leave our HIGH GPU priority with nothing queued to prioritise). Apollo's capture thread is
    // likewise CRITICAL.
    crate::punktfunk1::boost_thread_priority(true);

    // Capture the EXISTING SudoVDA output by GDI name / target id — do NOT create one (the host owns
    // the virtual output + its isolate/restore; a second topology owner breaks DDA recovery).
    let target = WinCaptureTarget {
        adapter_luid: 0,
        gdi_name: opts.gdi_name.clone(),
        target_id: opts.target_id,
    };
    let mut cap =
        WgcCapturer::open(target, Some((opts.width, opts.height, opts.fps))).context("WGC open")?;
    cap.set_active(true);

    // O3 present-trigger experiment: spawn a thread that PRESENTS a D3D swapchain to the virtual
    // display (a present SOURCE), testing whether that — unlike WGC's READ — makes the OS assign the
    // driver's IddCx swap-chain (so the driver's run_core runs + can push). Gated; diagnostic.
    if std::env::var_os("PUNKTFUNK_PRESENT_TRIGGER").is_some() {
        let (w, h) = (opts.width, opts.height);
        std::thread::Builder::new()
            .name("pf-present-trigger".into())
            .spawn(move || {
                tracing::info!("present-trigger: starting D3D present loop on the virtual display");
                if let Err(e) = unsafe { present_trigger(w, h) } {
                    tracing::warn!("present-trigger error: {e:#}");
                }
            })
            .ok();
    }

    // First frame establishes the real dimensions + whether the desktop is HDR (the encoder derives
    // Main10/HDR from the frame's PixelFormat::Rgb10a2). Then open NVENC on the capture device.
    let first = cap.next_frame().context("first WGC frame")?;
    let (w, h) = (first.width, first.height);
    let mut enc = encode::open_video(
        Codec::H265,
        first.format,
        w,
        h,
        opts.fps,
        opts.bitrate_kbps as u64 * 1000,
        false,          // not cuda
        opts.bit_depth, // 8, or 10 = Main10 (HDR auto-upgrades from the Rgb10a2 frame regardless)
    )
    .context("open NVENC")?;

    // Control channel: the host writes a single byte on our stdin to force an IDR (client decode
    // recovery), mirroring `enc.request_keyframe()` in the single-process path. A reader thread sets
    // a flag the encode loop checks; stdin EOF (host gone) just stops the thread.
    let kf = Arc::new(AtomicBool::new(false));
    {
        let kf = kf.clone();
        std::thread::Builder::new()
            .name("wgc-helper-ctl".into())
            .spawn(move || {
                let mut stdin = std::io::stdin();
                let mut byte = [0u8; 1];
                while let Ok(n) = stdin.read(&mut byte) {
                    if n == 0 {
                        break; // host closed our stdin
                    }
                    if byte[0] == CTL_KEYFRAME {
                        kf.store(true, Ordering::Relaxed);
                    }
                }
            })
            .ok();
    }

    // Binary stdout — lock it once + write framed AUs. A short write / broken pipe means the host
    // (parent) went away → exit cleanly so the host's relaunch watchdog can respawn us.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // FIXED-CADENCE encode loop (mirrors the single-process `punktfunk1::virtual_stream` loop). The
    // host runs as SYSTEM and relays our AUs; to deliver a STEADY `fps` to the client (the "fixed 240"
    // goal) we must NOT gate on WGC's content-driven FrameArrived — `WgcCapturer::next_frame` blocks up
    // to its ~8 ms static-repeat timeout when the desktop is quiet, capping a barely-changing desktop
    // ~125 fps regardless of the GPU. Instead we pace to `1/fps` and take the FRESHEST frame with the
    // non-blocking `try_latest`, repeating the last one when nothing newer arrived. Depth-1: NVENC's
    // `poll` (lock_bitstream) blocks until the just-submitted frame is encoded, so exactly one frame is
    // in flight per iteration. A deeper pipeline was measured to only stack latency under a
    // GPU-saturating game (the encodes serialize on the contended GPU anyway) — the in-game lever is
    // the GPU scheduling priority the SYSTEM host stamps on us, not pipeline depth.
    let interval = std::time::Duration::from_secs_f64(1.0 / opts.fps.max(1) as f64);

    let perf = crate::config::config().perf;
    let mut frames = 0u64;
    let mut repeats = 0u64; // frames where no newer capture had arrived (duplicate re-encode)
    let mut cap_ns = 0u64; // time in try_latest (capture + video-processor convert)
    let mut encode_ns = 0u64; // time blocked in lock_bitstream
    let mut write_ns = 0u64; // time writing the AU to the stdout pipe (relay backpressure)
    let mut window = std::time::Instant::now();

    // `frame` is held across iterations and repeated when `try_latest` has nothing newer, so a static
    // desktop still clocks `fps`. The capturer's held-set / output ring keep its texture alive across
    // the repeat; reassigning `frame` on a fresh capture drops the prior one (already drained by poll).
    let mut frame = first;
    let mut next = std::time::Instant::now();
    loop {
        if kf.swap(false, Ordering::Relaxed) {
            enc.request_keyframe();
        }
        // Freshest captured frame, or repeat the last (no new composition: static desktop / between a
        // game's presents). Non-blocking, so the cadence is OURS, not WGC's event rate.
        let t0 = std::time::Instant::now();
        match cap.try_latest().context("WGC try_latest")? {
            Some(f) => frame = f,
            None => repeats += 1,
        }
        if perf {
            cap_ns += t0.elapsed().as_nanos() as u64;
        }
        enc.submit(&frame).context("encoder submit")?;
        // Drain the just-submitted frame. NVENC's poll blocks in lock_bitstream until it's encoded, so
        // this returns exactly one AU (then None) — depth-1, no accumulation.
        loop {
            let p0 = std::time::Instant::now();
            let polled = enc.poll().context("encoder poll")?;
            if perf {
                encode_ns += p0.elapsed().as_nanos() as u64;
            }
            let Some(au) = polled else { break };
            let w0 = std::time::Instant::now();
            let wrote = write_au(&mut out, &au);
            if perf {
                write_ns += w0.elapsed().as_nanos() as u64;
            }
            if wrote.is_err() {
                tracing::info!("WGC helper: stdout closed (host gone) — exiting");
                return Ok(());
            }
        }
        // Pace to this frame's due time. If we're already past it (encode couldn't keep up under a
        // GPU-saturating game), skip the sleep and re-baseline so we don't spiral into catch-up.
        next += interval;
        match next.checked_duration_since(std::time::Instant::now()) {
            Some(d) => std::thread::sleep(d),
            None => next = std::time::Instant::now(),
        }

        if perf {
            frames += 1;
            let since = window.elapsed();
            if since.as_secs() >= 2 {
                let secs = since.as_secs_f64();
                let per = |ns: u64| format!("{:.2}", ns as f64 / frames as f64 / 1e6);
                tracing::info!(
                    fps = format!("{:.1}", frames as f64 / secs),
                    repeats,
                    cap_ms = per(cap_ns),
                    encode_ms = per(encode_ns),
                    write_ms = per(write_ns),
                    "WGC helper perf (fixed-cadence depth-1; encode_ms=lock_bitstream; repeats=duplicated frames)"
                );
                frames = 0;
                repeats = 0;
                cap_ns = 0;
                encode_ns = 0;
                write_ns = 0;
                window = std::time::Instant::now();
            }
        }
    }
}

fn write_au(out: &mut impl Write, au: &encode::EncodedFrame) -> std::io::Result<()> {
    out.write_all(&AU_MAGIC.to_le_bytes())?;
    out.write_all(&(au.data.len() as u32).to_le_bytes())?;
    out.write_all(&au.pts_ns.to_le_bytes())?;
    out.write_all(&[au.keyframe as u8])?;
    out.write_all(&au.data)?;
    out.flush()
}

/// O3 present-trigger experiment (see the gated call in `run`). Creates a small swapchain-backed
/// window on the virtual display (the CCD-isolated primary) and presents continuously — an active
/// present SOURCE on the display — to test whether that makes the OS assign the driver's IddCx
/// swap-chain (which WGC's read does not). Runs forever on its own thread.
///
/// # Safety
/// Win32/D3D11 FFI; called once on a dedicated helper thread.
unsafe fn present_trigger(disp_w: u32, disp_h: u32) -> Result<()> {
    use windows::core::{w, Interface};
    use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView,
        ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{
        IDXGIAdapter, IDXGIDevice, IDXGIFactory2, DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC1,
        DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW,
        ShowWindow, MSG, PM_REMOVE, SW_SHOWNOACTIVATE, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOPMOST,
        WS_POPUP, WS_VISIBLE,
    };

    unsafe extern "system" fn wndproc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        DefWindowProcW(h, m, wp, lp)
    }

    let hinst: HMODULE = GetModuleHandleW(None)?;
    let cls = w!("pfPresentTrigger");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinst.into(),
        lpszClassName: cls,
        ..Default::default()
    };
    RegisterClassW(&wc);
    // Small window at the top-left of the (primary = virtual) display so it barely obscures the
    // captured desktop; topmost + no-activate so it doesn't steal focus.
    let win_w = disp_w.min(96) as i32;
    let win_h = disp_h.min(96) as i32;
    let hwnd: HWND = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        cls,
        w!("pf-present"),
        WS_POPUP | WS_VISIBLE,
        0,
        0,
        win_w,
        win_h,
        None,
        None,
        Some(hinst.into()),
        None,
    )?;
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )?;
    let device = device.context("present-trigger d3d11 device")?;
    let context = context.context("present-trigger d3d11 context")?;

    let dxgi_dev: IDXGIDevice = device.cast()?;
    let adapter: IDXGIAdapter = dxgi_dev.GetAdapter()?;
    let factory: IDXGIFactory2 = adapter.GetParent()?;
    let scd = DXGI_SWAP_CHAIN_DESC1 {
        Width: win_w as u32,
        Height: win_h as u32,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        ..Default::default()
    };
    let swapchain = factory.CreateSwapChainForHwnd(&device, hwnd, &scd, None, None)?;
    tracing::info!("present-trigger: swapchain created on the virtual display; presenting");

    let mut frame = 0u32;
    loop {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = DispatchMessageW(&msg);
        }
        let back: ID3D11Texture2D = swapchain.GetBuffer(0)?;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
        let rtv = rtv.context("present-trigger rtv")?;
        let c = (frame % 120) as f32 / 120.0;
        context.ClearRenderTargetView(&rtv, &[c, 0.1, 0.2, 1.0]);
        let _ = swapchain.Present(1, DXGI_PRESENT(0));
        frame = frame.wrapping_add(1);
    }
}
