//! One-shot IDD-push construction: adapter, HDR, unnamed shared objects, channel
//! delivery, cursor opt-in, and the bounded first-frame gate.
//!
//! Types that exist only here: [`SharedObjectSa`], [`AttachTexFail`]. Steady-state
//! capture (`try_consume`, `repeat_last`, pollers, `Capturer`) stays in the parent.
//! A `#[path]` child sees the parent's private items through `use super::*`.
//! Evidence: `design/idd-push-security.md`.

use super::*;

/// `SECURITY_ATTRIBUTES` for unnamed header/event/texture objects: SDDL
/// `D:P(A;;GA;;;SY)`, protected, `bInheritHandle: false`.
///
/// The driver never opens by name; it receives duplicated handles (access travels
/// with the handle; `OpenSharedResource*` does not re-check the DACL). See
/// `design/idd-push-security.md`.
///
/// RAII over the `LocalAlloc` descriptor. `sa.lpSecurityDescriptor` points at
/// `psd`; [`as_ptr`](Self::as_ptr) only lends a borrow, so the attributes cannot
/// outlive this value. Moving is fine — the pointer targets the heap, not a field.
struct SharedObjectSa {
    sa: SECURITY_ATTRIBUTES,
    psd: PSECURITY_DESCRIPTOR,
}

impl SharedObjectSa {
    fn new() -> Result<Self> {
        let mut psd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the `w!()` literal is the SDDL source; the call writes its
        // `LocalAlloc` descriptor into this live `psd`; `?` rejects failure
        // before `psd` is read.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:P(A;;GA;;;SY)"),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )
            .context("build SDDL for IDD-push shared objects")?;
        }
        Ok(Self {
            sa: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: psd.0,
                bInheritHandle: false.into(),
            },
            psd,
        })
    }

    /// Borrowed from this owner; the descriptor must outlive the create call.
    fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.sa
    }
}

impl Drop for SharedObjectSa {
    fn drop(&mut self) {
        // SAFETY: `psd` is the descriptor this value's constructor allocated and
        // nothing else owns it. `LocalFree` runs once (`Drop` once; `as_ptr` only
        // lends a borrow of `sa`).
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.psd.0)));
        }
    }
}

impl IddPushCapturer {
    /// `RING_LEN` unnamed keyed-mutex textures at `format`. The driver reaches
    /// each only via the [`ChannelBroker`] duplicate after the ring is published.
    pub(super) fn create_ring_slots(
        device: &ID3D11Device,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
    ) -> Result<Vec<HostSlot>> {
        // SAFETY: every D3D11/DXGI call is `?`-checked on the live `device` borrow,
        // over initialized stack descriptors and live out-params. `sa` owns the
        // descriptor for the whole loop. `OwnedHandle::from_raw_handle` adopts the
        // unique NT handle `CreateSharedHandle` just minted for this slot.
        unsafe {
            let sa = SharedObjectSa::new()?;
            let mut slots = Vec::new();
            for _ in 0..RING_LEN {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    // Composition format: the driver's CopyResource and format-guard both require it.
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
                        | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0)
                        as u32,
                };
                let mut tex: Option<ID3D11Texture2D> = None;
                device
                    .CreateTexture2D(&desc, None, Some(&mut tex))
                    .context("CreateTexture2D(IDD-push ring slot)")?;
                let tex = tex.context("null ring texture")?;
                let res1: IDXGIResource1 = tex.cast()?;
                let shared = res1
                    .CreateSharedHandle(
                        Some(sa.as_ptr()),
                        DXGI_SHARED_RESOURCE_RW,
                        PCWSTR::null(), // unnamed: driver gets the broker duplicate, not a name
                    )
                    .context("CreateSharedHandle(IDD-push ring slot)")?;
                let shared = OwnedHandle::from_raw_handle(shared.0 as _);
                let mutex: IDXGIKeyedMutex = tex.cast()?;
                let mut srv: Option<ID3D11ShaderResourceView> = None;
                device
                    .CreateShaderResourceView(&tex, None, Some(&mut srv))
                    .context("CreateShaderResourceView(IDD-push ring slot)")?;
                let srv = srv.context("null slot srv")?;
                slots.push(HostSlot {
                    tex,
                    mutex,
                    shared,
                    srv,
                });
            }
            Ok(slots)
        }
    }

    /// Open the IDD-push capturer. Success attaches `keepalive` (the capturer
    /// owns the virtual display). Failure returns it so the caller retires or
    /// retries the monitor — this function never tears the display down.
    ///
    /// Failure includes the driver not attaching within a few seconds (hybrid-GPU
    /// render mismatch). There is no fallback capture path.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        want_hdr: bool,
        ten_bit_sdr: bool,
        want_444: bool,
        pyrowave: bool,
        keepalive: Box<dyn Send>,
        sender: crate::FrameChannelSender,
        cursor_sender: Option<crate::CursorChannelSender>,
        cursor_forward: Option<crate::CursorForwardSender>,
    ) -> std::result::Result<Self, (anyhow::Error, Box<dyn Send>)> {
        // Idempotent: first capturer starts it so stall logs can correlate DWM holes
        // with OS display events for the session's life.
        pf_win_display::display_events::spawn_once();
        match Self::open_inner(
            target,
            preferred,
            want_hdr,
            ten_bit_sdr,
            want_444,
            pyrowave,
            sender,
            cursor_sender,
            cursor_forward,
        ) {
            Ok(mut me) => {
                me._keepalive = keepalive;
                Ok(me)
            }
            Err(e) => Err((e, keepalive)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn open_inner(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        want_hdr: bool,
        ten_bit_sdr: bool,
        want_444: bool,
        pyrowave: bool,
        sender: crate::FrameChannelSender,
        cursor_sender: Option<crate::CursorChannelSender>,
        cursor_forward: Option<crate::CursorForwardSender>,
    ) -> Result<Self> {
        // Ring lives on the driver's render adapter. `resolve_render_adapter_luid` is
        // the same pick SET_RENDER_ADAPTER pinned at monitor ADD. `target.adapter_luid`
        // is IddCx's DISPLAY adapter (`OsAdapterLuid`), last-resort only. On drift
        // (stale monitor, ignored SET_RENDER_ADAPTER) TEX_FAIL reports the real LUID.
        let luid = pf_gpu::resolve_render_adapter_luid().unwrap_or(LUID {
            LowPart: (target.adapter_luid & 0xffff_ffff) as u32,
            HighPart: (target.adapter_luid >> 32) as i32,
        });
        match Self::open_on(
            target.clone(),
            preferred,
            want_hdr,
            ten_bit_sdr,
            want_444,
            pyrowave,
            luid,
            sender.clone(),
            cursor_sender.clone(),
            cursor_forward.clone(),
        ) {
            Ok(me) => Ok(me),
            Err(e) => {
                // One TEX_FAIL rebind: the driver stamped the adapter it actually renders
                // on. Pipeline retries would hit the same mismatch.
                let driver_luid = e
                    .downcast_ref::<AttachTexFail>()
                    .map(|tf| tf.driver_luid)
                    .filter(|d| *d != 0 && *d != crate::dxgi::pack_luid(luid));
                let Some(packed) = driver_luid else {
                    return Err(e);
                };
                let drv = LUID {
                    LowPart: (packed & 0xffff_ffff) as u32,
                    HighPart: (packed >> 32) as i32,
                };
                tracing::warn!(
                    ring_adapter = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    driver_adapter = format!("{:08x}:{:08x}", drv.HighPart, drv.LowPart),
                    "IDD push: ring/driver render-adapter mismatch — rebinding the ring to the \
                     driver's reported adapter"
                );
                Self::open_on(
                    target,
                    preferred,
                    want_hdr,
                    ten_bit_sdr,
                    want_444,
                    pyrowave,
                    drv,
                    sender,
                    cursor_sender,
                    cursor_forward,
                )
                .context("IDD-push rebind to the driver's reported render adapter")
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn open_on(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        want_hdr: bool,
        ten_bit_sdr: bool,
        want_444: bool,
        pyrowave: bool,
        luid: LUID,
        sender: crate::FrameChannelSender,
        cursor_sender: Option<crate::CursorChannelSender>,
        cursor_forward: Option<crate::CursorForwardSender>,
    ) -> Result<Self> {
        let (pw, ph, _hz) = preferred
            .context("IDD push needs the negotiated mode (WxH) to size the shared ring")?;
        // The complete CCD identity every display-global helper below selects paths by (the
        // packed LUID in the capture target is the IddCx display adapter's).
        let ccd =
            pf_win_display::win_display::CcdTargetKey::new(target.adapter_luid, target.target_id);
        // Size the ring to the display's ACTUAL current resolution if it differs from the negotiated mode:
        // a fullscreen game can hold the virtual display at a different mode (esp. across a reconnect), so
        // matching the actual mode lets the first frame flow instead of being dropped (game-capture bug
        // GB1). Falls back to the negotiated mode when the CCD read is unavailable.
        let (w, h) = pf_win_display::win_display::active_resolution(ccd).unwrap_or((pw, ph));
        if (w, h) != (pw, ph) {
            tracing::info!(
                target_id = target.target_id,
                negotiated = format!("{pw}x{ph}"),
                actual = format!("{w}x{h}"),
                "IDD push: sizing the ring to the display's actual mode (differs from negotiated)"
            );
        }
        // Composition is FP16 scRGB in advanced-color, BGRA otherwise. A 10-bit
        // client enables HDR here and the descriptor poller tracks mid-session
        // flips. An SDR client forces advanced color OFF and stays pinned, so
        // the encoder cannot emit in-band PQ to a client that asked for SDR.

        // SAFETY: one block over ring setup.
        // - `set_advanced_color`/`advanced_color_enabled` take a copied `u32` target
        //   id and return owned values; they borrow nothing from this stack.
        // - Factory/adapter/device/`SharedObjectSa`/`CreateFileMappingW`/`MapViewOfFile`/
        //   `CreateEventW`/`create_ring_slots` are `?`-checked. `sa` stays in scope
        //   for every create that borrows `as_ptr()`.
        // - The mapping is created and viewed at `bytes == size_of::<SharedHeader>().max(64)`;
        //   a null view `bail!`s (owned `map` then closes). The OS base is page-aligned,
        //   so `section.ptr::<SharedHeader>()` is aligned. `write_bytes` and the
        //   `(*header).field` stores stay inside `bytes` and never form `&mut`.
        // - Magic is published through `addr_of!((*header).magic) as *const AtomicU32`
        //   (no reference). The field is 4-aligned `u32`. The `Release` store after
        //   the `Release` fence is the handshake: prior writes before the driver
        //   may observe `MAGIC`.
        // - `broker.send` borrows this process's just-created header/event handles
        //   for that synchronous call only.
        // - `header` points into the OS mapping, not into `MappedSection`, so
        //   moving `section` into `me` leaves it valid (see `MappedSection`).
        unsafe {
            // SDR session: force advanced color OFF before sizing (leftover HDR on a
            // reused monitor, driver default, or host "Use HDR"). PyroWave CSC reads
            // 8-bit BGRA only; H.26x would emit P010+PQ from an FP16 ring to a client
            // that asked for SDR. HDR sessions enable below and ride FP16 scRGB.
            if !want_hdr {
                let _ = pf_win_display::win_display::set_advanced_color(ccd, false);
                let settle = Instant::now();
                while settle.elapsed() < Duration::from_millis(250) {
                    if pf_win_display::win_display::advanced_color_enabled(ccd) == Some(false) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                if pf_win_display::win_display::advanced_color_enabled(ccd) == Some(true) {
                    tracing::error!(
                        target = target.target_id,
                        pyrowave,
                        "IDD push: SDR session but advanced color (HDR) could NOT be turned off on the \
                         virtual display (a physical display forcing HDR?) — PyroWave will likely fail \
                         its first frame; H.26x would emit PQ the SDR-only client never asked for"
                    );
                } else {
                    tracing::info!(
                        target = target.target_id,
                        pyrowave,
                        settle_ms = settle.elapsed().as_millis() as u64,
                        "IDD push: SDR-negotiated session — advanced color forced OFF (SDR/BGRA composition)"
                    );
                }
            }
            // 10-bit: size FP16 from the successful set, not the CCD poll. A 250 ms
            // poll can still read SDR while the driver already composes FP16, which
            // mismatches and drops the first frames.
            let enabled_hdr =
                want_hdr && pf_win_display::win_display::set_advanced_color(ccd, true);
            if enabled_hdr {
                // Poll CCD instead of a fixed sleep; 250 ms ceiling. Timeout still
                // sizes FP16 from `enabled_hdr` — the set succeeded; stash/format-guard
                // absorbs a lagging driver compose flip.
                let hdr_settle = Instant::now();
                while hdr_settle.elapsed() < Duration::from_millis(250) {
                    if pf_win_display::win_display::advanced_color_enabled(ccd) == Some(true) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                tracing::debug!(
                    target_id = target.target_id,
                    settle_ms = hdr_settle.elapsed().as_millis() as u64,
                    "IDD push: advanced-color (HDR) enable settle"
                );
            }
            // A failed open-time read defaults to SDR (unless the 10-bit path enabled HDR above) —
            // there is no "last known" yet; the descriptor poller corrects a wrong guess mid-session.
            // An SDR-negotiated session (either codec) forced advanced color OFF above and composes
            // SDR unconditionally: `want_hdr` gates HDR so a client that advertised SDR-only is
            // never handed a PQ stream, even if a physical display forces HDR on (the descriptor
            // poller re-asserts OFF; PyroWave's format guard/stash absorbs any lingering FP16 compose).
            // Keep the raw observation so Downgrade point D below can say whether the read reported
            // OFF or failed outright — "we asked, it said no" and "we could not tell" have different
            // causes and different fixes.
            let observed_hdr = pf_win_display::win_display::advanced_color_enabled(ccd);
            let display_hdr = want_hdr && (enabled_hdr || observed_hdr.unwrap_or(false));
            // Negotiated 10-bit but advanced color did not enable: ring is SDR and
            // the encoder emits 8-bit BT.709 while Welcome said HDR. Loud — every
            // frame of this session is wrong until the descriptor poller sees HDR.
            if want_hdr && !display_hdr {
                tracing::error!(
                    target = target.target_id,
                    want_hdr = true,
                    set_advanced_color_returned = enabled_hdr,
                    observed_hdr = ?observed_hdr,
                    "IDD push: 10-bit HDR was negotiated but enabling advanced color on the \
                     virtual display FAILED — encoding 8-bit SDR while the client was told HDR \
                     (check the display driver / Windows HDR support on this box). \
                     observed_hdr=Some(false) ⇒ the display reports advanced colour OFF after the \
                     set; None ⇒ the CCD read itself failed"
                );
            }
            let ring_fmt = if display_hdr {
                DXGI_FORMAT_R16G16B16A16_FLOAT
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            };
            // Ring + NVENC on `luid`. Shared textures open only if the driver
            // swap-chain is on the same adapter (`open_inner` rebinds once if not).
            let factory: IDXGIFactory4 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapterByLuid(luid)
                .context("EnumAdapterByLuid(render adapter) for IDD push")?;
            let (device, context) = make_device(&adapter).context("make_device for IDD push")?;

            let sa = SharedObjectSa::new()?;
            // The full v3 layout. A v2 driver maps the whole section and reads only its 88-byte
            // prefix; the v3 tail is reachable to it only through `frame::v3_readable`, which its
            // version gate fails.
            let bytes = std::mem::size_of::<SharedHeader>();

            // Unnamed mapping: the driver receives a duplicated handle, not a name.
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(sa.as_ptr()),
                PAGE_READWRITE,
                0,
                bytes as u32,
                PCWSTR::null(),
            )
            .context("CreateFileMapping(IDD-push header)")?;
            // `MappedSection` RAII closes mapping + view even if we `bail!` on the view.
            let map = OwnedHandle::from_raw_handle(map.0 as _);
            let view = MapViewOfFile(
                HANDLE(map.as_raw_handle()),
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                bytes,
            );
            if view.Value.is_null() {
                bail!("MapViewOfFile failed for IDD-push header");
            }
            let section = MappedSection { handle: map, view };
            let generation = next_generation();
            let header = section.ptr::<SharedHeader>();
            std::ptr::write_bytes(header.cast::<u8>(), 0, bytes);
            (*header).version = VERSION;
            // What THIS host can act on (immunity plan WP4). The fence ring and the swap-chain
            // reset actuator stay off until WP7/WP13 implement them; a transport activates only
            // where `frame::negotiate` finds both sides agreeing.
            (*header).host_capabilities = pf_driver_proto::frame::CAP_RING_HEALTH_V3
                | pf_driver_proto::frame::CAP_SOURCE_SEQUENCE_QPC;
            (*header).generation = generation;
            (*header).ring_len = RING_LEN;
            (*header).width = w;
            (*header).height = h;
            // Composition format. The driver copies this into `ring_format` and
            // drops any surface that does not match.
            (*header).dxgi_format = ring_fmt.0 as u32;
            // Stamp `target_id` before magic; it never changes for this mapping.
            // The driver refuses a ring that names a different monitor, so a stash
            // cross-wire fails closed (`design/idd-push-security.md`).
            (*header).target_id = target.target_id;

            // Auto-reset, unnamed. Driver signals on each publish.
            let event = CreateEventW(Some(sa.as_ptr()), false, false, PCWSTR::null())
                .context("CreateEvent(IDD-push)")?;
            let event = OwnedHandle::from_raw_handle(event.0 as _);

            let slots = Self::create_ring_slots(&device, w, h, ring_fmt)?;

            // Magic last (Release): the ring is fully initialized before the driver
            // — which receives the channel after this — can observe MAGIC.
            std::sync::atomic::fence(Ordering::Release);
            (*(std::ptr::addr_of!((*header).magic) as *const AtomicU32))
                .store(MAGIC, Ordering::Release);

            // Duplicate header + event + slots into WUDFHost. All-or-nothing (the
            // broker reaps remote duplicates on failure). Fail the open if this
            // fails: without delivery the driver can never attach.
            let broker = ChannelBroker::open(target.wudf_pid, sender)?;
            broker
                .send(
                    target.target_id,
                    generation,
                    HANDLE(section.handle.as_raw_handle()),
                    HANDLE(event.as_raw_handle()),
                    &slots,
                )
                .context("deliver IDD-push frame channel to the driver")?;

            // CursorShm create + deliver. Non-fatal: without it the driver never
            // declares a hardware cursor, so this session composites the pointer.
            let cursor_shared = cursor_sender.as_ref().and_then(|send_cursor| {
                match cursor::CursorShared::create(ccd) {
                    Ok(cs) => {
                        // Shared helper: also re-delivers after a driver monitor re-arrival.
                        deliver_cursor_channel(&broker, target.target_id, &cs, send_cursor)
                            .then_some(cs)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "cursor section creation failed — the driver will not declare a \
                             hardware cursor, so this session cannot forward the pointer: {e:#}"
                        );
                        None
                    }
                }
            });
            // Sticky hardware-cursor declare from an earlier session still excludes
            // the pointer from DWM frames. No live channel this session ⇒ force
            // composite or there is no pointer at all. Gate on `cursor_shared`, not
            // `cursor_sender`: delivery is allowed to fail non-fatally.
            let composite_forced = target.cursor_excluded && cursor_shared.is_none();
            if composite_forced {
                tracing::info!(
                    target_id = target.target_id,
                    negotiated_channel = cursor_sender.is_some(),
                    "target carries an irrevocable hardware-cursor declare from an earlier \
                     desktop-mode session and this session has no LIVE cursor channel — the host \
                     composites the pointer into frames (forced, for the session's life). \
                     negotiated_channel=true ⇒ one was negotiated but its creation/delivery failed"
                );
            }
            // Same gate as the live channel. IddCx cannot deliver masked/monochrome
            // (`cursor_poll.rs`); forced-composite sessions need this as their only
            // shape/position source.
            let cursor_poll = (cursor_shared.is_some() || composite_forced).then(|| {
                // Safety of the CCD call: read-only QueryDisplayConfig over owned locals (same
                // call CursorShared::create makes) — already inside open_on's unsafe region.
                let rect = pf_win_display::win_display::source_desktop_rect(ccd).unwrap_or((
                    0,
                    0,
                    i32::MAX,
                    i32::MAX,
                ));
                cursor_poll::CursorPoller::spawn(ccd, rect)
            });
            // Previous session may have died on the secure desktop with desired
            // state `false`; delivery would then start undeclared. Fresh sessions
            // start declared; `poll_secure_desktop` re-disables if still locked.
            if let (Some(_), Some(fwd)) = (cursor_shared.as_ref(), cursor_forward.as_ref()) {
                if let Err(e) = fwd(true) {
                    tracing::debug!("cursor-forward reset at open failed (pre-v6 driver?): {e:#}");
                }
            }

            tracing::info!(
                target_id = target.target_id,
                wudf_pid = target.wudf_pid,
                render_luid = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                mode = format!("{w}x{h}"),
                display_hdr,
                want_hdr,
                ten_bit_sdr,
                want_444,
                ring_fp16 = display_hdr,
                // DXGI has already run (factory / EnumAdapterByLuid / make_device).
                // 0 ⇒ the win32u GPU-preference hook is inert this build — first
                // check on hybrid-GPU TEX_FAIL (`dxgi::install_gpu_pref_hook`).
                hybrid_hook_hits = crate::dxgi::hybrid_hook_hits(),
                // The diagnostic POSTURE, recorded once per session (immunity plan WP3): a field
                // log must say whether active micro-probes were running — they alter the very
                // path a disturbance report describes, so every report needs this A/B label.
                stall_probes = pf_host_config::config().stall_probes,
                "IDD push(host): created sealed ring + delivered the channel; waiting for the driver \
                 to attach + publish"
            );
            let mut me = Self {
                device,
                context,
                target_id: target.target_id,
                ccd,
                source_seq: 0,
                recovery: super::recovery::Supervisor::new(Instant::now()),
                section,
                header,
                event,
                broker,
                width: w,
                height: h,
                slots,
                generation,
                want_hdr,
                ten_bit_sdr,
                display_hdr,
                hdr_pin_warned: false,
                hdr_pin_failures: 0,
                want_444,
                pyrowave,
                pyro_fence: None,
                pyro_fence_handle: None,
                pyro_fence_value: 0,
                pyro_ring: Vec::new(),
                pyro_conv: None,
                pyro_last: None,
                desc_poller: DescriptorPoller::spawn(
                    ccd,
                    DisplayDescriptor {
                        hdr: display_hdr,
                        width: w,
                        height: h,
                    },
                ),
                desc_seq: 0,
                pending_desc: None,
                recovering_since: None,
                last_fresh: Instant::now(),
                last_liveness: Instant::now(),
                last_kick: Instant::now(),
                stall_watch: StallWatch::new(),
                offered_at_fresh: 0,
                max_hb_age_us: 0,
                cursor_last: None,
                cursor_gap_px: 0,
                cursor_pending_px: 0,
                cursor_sampled_at: Instant::now(),
                probes: pf_host_config::config()
                    .stall_probes
                    .then(super::probes::acquire),
                etw: super::dxgkrnl_etw::acquire(),
                out_ring: Vec::new(),
                out_idx: 0,
                video_conv: None,
                hdr_p010_conv: None,
                hdr_rgb10_conv: None,
                sdr_rgb10_conv: None,
                last_seq: 0,
                last_present: None,
                status_logged: false,
                cursor_shared,
                cursor_poll,
                cursor_sender,
                cursor_forward,
                secure_active: false,
                composite_cursor: composite_forced,
                composite_forced,
                cursor_blend: None,
                cursor_blend_failed: false,
                cursor_shm_latched: false,
                blend_scratch: None,
                last_blend_key: None,
                last_slot: None,
                sdr_white_scale: 1.0,
                // Taken before the first-frame gate so the display cannot idle off
                // while we wait; held until the capturer drops.
                _display_wake: pf_frame::session_tuning::DisplayWakeRequest::new(),
                // Placeholder. `open()` attaches the real keepalive only on success
                // so a failed open can hand it back.
                _keepalive: Box::new(()),
            };
            // Stamp both REALTIME GPU-priority opt-ins once per session. Stall
            // WARNs repeat them only when they fire, so a quiet stalling log
            // would otherwise omit the posture.
            tracing::info!(
                rt_gpu_driver = super::stall::rt_gpu_driver_posture(),
                rt_gpu_host = super::stall::rt_gpu_host_posture(),
                "GPU-priority posture for this capture session"
            );
            // Query once here, not from the blend (that holds the slot keyed mutex;
            // `refresh_sdr_white_scale`). No-op on SDR.
            me.refresh_sdr_white_scale();
            // Attach + first frame, or fail the open. TEX_FAIL and attach-but-no-frames
            // must not wait for `next_frame`'s 20 s black-then-bail.
            me.wait_for_attach()?;
            Ok(me)
        }
    }

    /// Bounded wait until the driver is `DRV_STATUS_OPENED` **and** has published
    /// a first frame; else fail so the caller can retire the display.
    ///
    /// Attach-only would miss a fullscreen game that left a format/size the
    /// driver's `publish()` guard rejects: the driver attaches and drops every
    /// frame, and `next_frame` then blacks out for 20 s. A stash-capable driver
    /// republishes on attach (`FrameStash` in `frame_transport.rs`), so a healthy
    /// idle desktop clears in milliseconds. Otherwise DWM compose after activate
    /// (~1 s) plus the kick below; no frame in the window is genuinely broken.
    fn wait_for_attach(&self) -> Result<()> {
        // Our stamp; nothing legitimate rewrites it. A mismatch is a host-side
        // stash/capturer cross-wire — the same class the driver refuses from
        // the other end.

        // SAFETY: in-bounds, aligned u32 read of the live, owned shared-header
        // mapping (same access as the `driver_status` read below); no reference
        // into the shared region is formed.
        let stamped = unsafe { (*self.header).target_id };
        if stamped != self.target_id {
            bail!(
                "IDD-push: our ring header names target {stamped} but this capturer serves target \
                 {} — host-side ring↔monitor cross-wire (bug); failing the open",
                self.target_id
            );
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        // Stash republish should clear this in milliseconds. 600 ms lets the
        // post-activate compose and stash path run first; then kick, because DWM
        // only presents a display something dirtied (idle desktop → E_PENDING).
        // Log the kick: it means stash did not republish (pre-stash / empty).
        let mut next_kick = Instant::now() + Duration::from_millis(600);
        loop {
            // SAFETY: `self.header` points into this capturer's live mapping
            // (`>= size_of::<SharedHeader>()`, page-aligned). The field read is
            // in-bounds and aligned; no reference into the shared region is formed.
            // Aligned `u32` cannot tear. `driver_status` is diagnostics; the
            // handshake is atomic `magic`/`latest`.
            let st = unsafe { (*self.header).driver_status };
            if st == DRV_STATUS_TEX_FAIL {
                // Driver writes its render LUID before the texture opens
                // (`frame_transport.rs`), so it is valid on TEX_FAIL.
                let (_, detail, lo, hi) = self.driver_diag();
                return Err(anyhow::Error::new(AttachTexFail {
                    detail,
                    driver_luid: ((hi as i64) << 32) | (lo as i64 & 0xffff_ffff),
                }));
            }
            if st == DRV_STATUS_NO_DEVICE1 {
                // SAFETY: in-bounds aligned `u32` diagnostic on the owned live mapping;
                // no reference into the shared region is formed.
                let detail = unsafe { (*self.header).driver_status_detail };
                bail!(
                    "IDD-push driver failed to attach (driver_status={st} detail=0x{detail:08x} — \
                     the driver has no ID3D11Device1 to open shared resources)"
                );
            }
            if st == DRV_STATUS_BIND_FAIL {
                // SAFETY: in-bounds aligned `u32` diagnostic on the owned live mapping;
                // no reference into the shared region is formed.
                let claimed = unsafe { (*self.header).driver_status_detail };
                bail!(
                    "IDD-push driver REFUSED the ring↔monitor binding (DRV_STATUS_BIND_FAIL: the \
                     delivered ring names target {claimed}, the monitor is {}) — host \
                     stash/delivery cross-wire (bug); failing the open loudly (proto v3 §3.2)",
                    self.target_id
                );
            }
            // `seq != 0` is the first publish; attach alone is not enough.
            if st == DRV_STATUS_OPENED && frame::FrameToken::unpack(self.latest()).seq != 0 {
                return Ok(());
            }
            if Instant::now() >= next_kick {
                // Kick means stash did not republish (pre-stash driver or empty stash).
                tracing::debug!(
                    target_id = self.target_id,
                    driver_status = st,
                    "IDD push: no first frame after attach delivery — falling back to a synthetic \
                     compose kick (stash-capable drivers republish instantly; old driver?)"
                );
                // May BLOCK this thread ~35 ms (the cursor-on-a-sibling-display branch — see
                // `kick_dwm_compose`'s COST note). Fine here: we are inside the open-time
                // first-frame gate, so no frames are flowing yet.
                kick_dwm_compose(self.ccd);
                next_kick = Instant::now() + Duration::from_millis(800);
            }
            if Instant::now() > deadline {
                bail!(
                    "IDD-push: no frame published within 4s (despite compose kicks) — {}; \
                     falling back",
                    self.no_first_frame_diagnosis(st)
                );
            }
            // Wake on the frame-ready event. 20 ms timeout keeps `driver_status`
            // polls live (status writes do not signal). Consuming a signal here
            // is fine: `next_frame` trusts atomic `latest`, not the event.

            // SAFETY: `self.event` is this capturer's owned, live auto-reset event
            // handle; `WaitForSingleObject` only reads it and the 20 ms timeout
            // bounds the wait.
            let _ = unsafe { WaitForSingleObject(HANDLE(self.event.as_raw_handle()), 20) };
        }
    }

    /// Name a first-frame timeout from `driver_status` plus the OPENED detail
    /// word (`pack_opened_detail`). The three no-frames states look identical
    /// from the host and have disjoint fixes. Appends a console-session hint
    /// when display writes and input kicks cannot work from this session.
    fn no_first_frame_diagnosis(&self, st: u32) -> String {
        let what = match st {
            DRV_STATUS_NONE => "the driver never attached — the channel delivery was never \
                 consumed, so the OS ran no swap-chain worker for this monitor (display not \
                 composed at all: console display-off / modern standby, or the mode commit \
                 never reached the adapter)"
                .to_string(),
            DRV_STATUS_OPENED => {
                // SAFETY: in-bounds aligned u32 diagnostic on the owned live mapping
                // (same access as `driver_status` in the caller); no reference into
                // the shared region is formed.
                let detail = unsafe { (*self.header).driver_status_detail };
                match unpack_opened_detail(detail) {
                    Some((0, _)) => "driver attached with a live swap-chain, but DWM composed \
                         ZERO frames — an undamaged or powered-off desktop, and the compose \
                         kicks didn't bite (synthetic input is blocked on the secure desktop)"
                        .to_string(),
                    Some((offered, mismatched)) => format!(
                        "driver attached and DWM composed {offered} frame(s), but none matched \
                         the ring — {mismatched} dropped for a size/format mismatch (the \
                         display's actual mode differs from what the host sized the ring to: \
                         a mid-open mode-set, a fullscreen game, or a stale GDI view)"
                    ),
                    // Pre-detail driver never stamps the live bit; do not guess.
                    None => "driver attached but published nothing; this pf-vdisplay build \
                         predates attach diagnostics, so the cause can't be named — update the \
                         driver for a precise line here"
                        .to_string(),
                }
            }
            other => format!("driver_status={other} (unexpected at this point)"),
        };
        match pf_win_display::console_session_mismatch() {
            Some((own, console)) => format!(
                "{what} [host is in session {own} but the console is session {console} — display \
                 writes and input kicks cannot work from a non-console session; reconnect the \
                 console or run via the installed service]"
            ),
            None => what,
        }
    }
}

/// TEX_FAIL as a typed error so `open_inner` can downcast and rebind once.
/// `driver_luid` is the packed adapter the driver's swap-chain actually renders on.
#[derive(Debug)]
struct AttachTexFail {
    detail: u32,
    driver_luid: i64,
}

impl std::fmt::Display for AttachTexFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "IDD-push driver failed to attach (driver_status={DRV_STATUS_TEX_FAIL} \
             detail=0x{:08x}): it could not open the ring textures — its swap-chain renders on \
             adapter {:08x}:{:08x}, not the ring's (render-adapter mismatch)",
            self.detail,
            (self.driver_luid >> 32) as i32,
            (self.driver_luid & 0xffff_ffff) as u32,
        )
    }
}

impl std::error::Error for AttachTexFail {}
