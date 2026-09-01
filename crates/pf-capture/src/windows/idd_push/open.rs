//! IDD-push CONSTRUCTION: everything that runs once, before frames flow.
//!
//! The sealed channel's whole bring-up — the render-adapter resolution and its one TEX_FAIL rebind,
//! the advanced-colour (HDR) negotiation, the shared header + ring + event creation, the channel
//! delivery, the cursor-channel/poller opt-in, and the bounded first-frame gate — plus the two types
//! that exist only for it ([`SharedObjectSa`], [`AttachTexFail`]).
//!
//! Split out of `idd_push.rs` in sweep Phase 5.4. The steady state (`try_consume`, `repeat_last`,
//! the pollers, the `Capturer` impl) deliberately stays with the parent: this file is the part you
//! read when a session will not START, and the parent is the part you read when one stops flowing.
//! A `#[path]` child sees the parent's private items through `use super::*`, so nothing had to be
//! made more visible to move here.

use super::*;

/// Build a `SECURITY_ATTRIBUTES` granting GENERIC_ALL to **SYSTEM only** — `D:P(A;;GA;;;SY)`, protected
/// (no inherited ACEs), `bInheritHandle: false`. The sealed channel makes this the strictly-minimal
/// DACL: the objects are UNNAMED and the driver reaches them via **duplicated handles** (which carry the
/// source handle's access — `OpenSharedResourceByName`/`OpenSharedResource1` on a handle does not
/// re-check the object DACL against the opener), so the pf_vdisplay WUDFHost (LocalService) no longer
/// needs a DACL ACE. Dropping the `LS` ACE removes the last theoretical surface where a leaked handle or
/// a name-grown-by-accident could be opened by the (many-service-shared) LocalService SID. Empirically
/// confirmed unreachable regardless: a LocalService token is DACL-denied `OpenProcess` on the WUDFHost
/// (`PROCESS_DUP_HANDLE`/`VM_READ`/even `QUERY_LIMITED` → ACCESS_DENIED, tested on the RTX box
/// 2026-07-03), so it cannot dup the handles out either. History: `Global\`-named + world-openable
/// (`WD`, security-review 2026-06-28 #5) → SY+LS-scoped → nameless → now SY-only. See
/// `design/idd-push-security.md`.
///
/// RAII, because the descriptor is a `LocalAlloc` the caller must `LocalFree` and the previous
/// `(SECURITY_ATTRIBUTES, PSECURITY_DESCRIPTOR)` tuple never did — leaking it twice per open (once
/// for the header section, once for the frame-ready event) and again on every ring recreate. Pairing
/// them in one owner also enforces the "descriptor must outlive the attributes" rule structurally:
/// `sa.lpSecurityDescriptor` points at the allocation and [`as_ptr`](Self::as_ptr) only lends a
/// borrow, so the attributes cannot escape this value's lifetime. Moving the struct is fine — the
/// pointer targets the heap allocation, not a field.
struct SharedObjectSa {
    sa: SECURITY_ATTRIBUTES,
    psd: PSECURITY_DESCRIPTOR,
}

impl SharedObjectSa {
    fn new() -> Result<Self> {
        let mut psd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `ConvertStringSecurityDescriptorToSecurityDescriptorW` reads the `w!()` literal and
        // writes the descriptor it allocates into the live local `psd`; `?` rejects a failure before
        // `psd` is read.
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

    /// The `SECURITY_ATTRIBUTES` to hand a create call, borrowed from this owner.
    fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.sa
    }
}

impl Drop for SharedObjectSa {
    fn drop(&mut self) {
        // SAFETY: `psd` is the descriptor `ConvertStringSecurityDescriptorToSecurityDescriptorW`
        // allocated for this value and nothing else owns it; `LocalFree` releases it exactly once
        // (this `Drop` runs once, and `as_ptr` only ever lends a borrow of `sa`).
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.psd.0)));
        }
    }
}

impl IddPushCapturer {
    /// Create the `RING_LEN` shared keyed-mutex textures for one ring generation, at `format` (matched
    /// to the display's composition format — FP16 in HDR, BGRA in SDR). Each is shared through an
    /// UNNAMED NT handle (nothing to open by name — the sealed channel); the driver reaches it only via
    /// the duplicate the [`ChannelBroker`] sends after the ring is published.
    pub(super) fn create_ring_slots(
        device: &ID3D11Device,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
    ) -> Result<Vec<HostSlot>> {
        // SAFETY: every D3D11/DXGI call is `?`-checked on the live `device` borrow, over
        // fully-initialized stack descriptors and live out-params; `&sa` stays valid for the whole loop
        // because `_psd`, the security descriptor backing it, is held in scope alongside.
        // `OwnedHandle::from_raw_handle` adopts the handle `CreateSharedHandle` JUST minted for this
        // slot — a unique, still-open NT handle owned by this process — making the slot its sole owner.
        unsafe {
            let sa = SharedObjectSa::new()?;
            let mut slots = Vec::new();
            for _ in 0..RING_LEN {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    // Match the OS-composed swap-chain surfaces so the driver's CopyResource into the slot +
                    // its format-guard both succeed.
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
                        PCWSTR::null(), // UNNAMED — reachable only through the broker's duplicate
                    )
                    .context("CreateSharedHandle(IDD-push ring slot)")?;
                // Own the shared handle so the slot's `Drop` closes it via RAII (was a manual `CloseHandle`).
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

    /// Open the IDD-push capturer. On success the caller's `keepalive` is attached (the capturer owns the
    /// virtual display); on FAILURE the keepalive is handed BACK so the caller decides the display's fate
    /// itself — retire it, or reuse the monitor for a retry — instead of this function tearing it down
    /// (audit §5.1 — no more 20 s black bail). There is no second capture path to fall back TO: DDA was
    /// removed (see `lib.rs`), and `punktfunk-host`'s caller drops the returned keepalive under
    /// `.context("IDD-push capture open (no fallback)")`. "Failure" includes the
    /// driver not attaching to the ring within a few seconds (e.g. a hybrid-GPU render mismatch).
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
        // The stall-attribution listener (idempotent): started with the first IDD-push capturer so
        // the stall log can correlate DWM holes with OS display events for the session's lifetime.
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
        // The ring MUST live on the adapter the driver's swap-chain renders on. Primary: the
        // selected render GPU — the same pick SET_RENDER_ADAPTER pinned the driver to at monitor
        // ADD, so on a healthy box they agree, and NVENC gets a device on a real GPU adapter.
        // (`target.adapter_luid` is NOT that adapter: the ADD reply carries
        // `IDARG_OUT_MONITORARRIVAL.OsAdapterLuid` = the IddCx DISPLAY adapter — verified
        // on-glass; it stays a last-resort fallback for a pickerless box only.) When the pick and
        // the driver HAVE drifted — identical twin GPUs whose max-VRAM tie moved between ADD and
        // this open, or a stale kept monitor across an adapter re-init — the driver reports
        // TEX_FAIL plus the adapter it actually renders on, and the rebind below reopens on that.
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
                // Self-heal a render-adapter mismatch ONCE: on TEX_FAIL the driver has reported the
                // adapter its swap-chain ACTUALLY renders on (a stale monitor across an adapter
                // re-init, or a driver that ignored SET_RENDER_ADAPTER). Rebinding the ring to that
                // adapter beats failing the session — the outer pipeline retries would repeat the
                // exact same mismatch.
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
        // The driver composes the virtual display in FP16 (R16G16B16A16_FLOAT scRGB) when the display is
        // in advanced-color (HDR) mode, and 8-bit BGRA otherwise (per swap_chain_processor.rs + the
        // COMMIT_MODES2 colorspace/rgb_bpc log). For a 10-bit-capable client we PROACTIVELY enable
        // advanced color so HDR streams without the user toggling anything, then TRACK the display's
        // actual mode (a mid-session "Use HDR" flip; the driver's format-guard drops a mismatch), polling
        // the live state here and on every recreate. An SDR-only client instead forces advanced color OFF
        // and is PINNED there (below + the descriptor poller), so the SDR negotiation is honored and the
        // encoder never emits the in-band PQ upgrade to a client that asked for SDR.
        // SAFETY: one block over the whole ring setup; every operation in it is sound:
        // - `set_advanced_color`/`advanced_color_enabled` are `unsafe fn`s taking only a copy of the plain
        //   `u32` target id; they read/flip CCD display config and return owned values, borrowing nothing.
        // - `CreateDXGIFactory1`, `EnumAdapterByLuid`, `make_device`, `SharedObjectSa::new`,
        //   `CreateFileMappingW`, `MapViewOfFile`, `CreateEventW`, and `create_ring_slots` are all
        //   `?`-checked, so every returned interface/handle/view is non-error before use;
        //   `sa.as_ptr()`/`&adapter`/`&device` are live borrows that outlive each synchronous call, and
        //   `sa.lpSecurityDescriptor` stays valid because the owning `SharedObjectSa` is held in scope
        //   for the whole block (and frees the descriptor on the way out).
        // - The header mapping is created AND viewed at `bytes == size_of::<SharedHeader>().max(64)`; the
        //   view's null is checked (`bail!` on failure, after which the owned `map` closes the mapping). The
        //   OS view base is page-aligned, so `section.ptr::<SharedHeader>()` is suitably aligned for a
        //   `SharedHeader`, and `write_bytes(.., 0, bytes)` plus the `(*header).field = ..` writes all stay
        //   within those `bytes` and write THROUGH the raw pointer without forming any `&mut`.
        // - The `magic` publish stores through `addr_of!((*header).magic) as *const AtomicU32`: `addr_of!`
        //   takes the field address without a reference; the field is a 4-aligned `u32` (valid for
        //   `AtomicU32`), and the `Release` store after the `Release` fence is the cross-process handshake
        //   that orders all preceding writes before the driver may observe `MAGIC`.
        // - `broker.send` requires live `header`/`event` handles of this process: both borrow the just-
        //   created owned section/event for the duration of that synchronous call.
        // - `header` points into the OS mapping, NOT into the `MappedSection` struct, so moving `section`
        //   into `me` leaves it valid (see the `MappedSection` doc comment).
        unsafe {
            // An SDR-NEGOTIATED session (either codec) must run on an SDR (BGRA) composition, so
            // actively turn advanced color OFF — undoing any leftover HDR state from a prior 10-bit
            // session on a reused/lingering monitor, the driver's default, or the host's global
            // "Use HDR" — and settle before sizing the ring. Non-optional for two reasons:
            //   - PyroWave: its CSC reads 8-bit BGRA and the NVIDIA D3D11 VideoProcessor can't ingest
            //     the FP16 ring at all.
            //   - H.26x: off an HDR composition the capturer emits P010 and the encoder stamps
            //     Main10 + BT.2020 PQ from the pixel format alone (the in-band HDR upgrade), sending a
            //     10-bit PQ stream to a client that advertised SDR-only ("HDR off = never send me
            //     10-bit"). On a client whose monitor is HDR-capable but has "Use HDR" off, that PQ
            //     lands on an SDR desktop and blows out — the composition must honor the negotiation.
            // An HDR-negotiated (10-bit) session instead enables HDR below and rides the FP16 scRGB
            // ring (design/pyrowave-444-hdr.md Phase 3 for PyroWave; the H.26x P010 path otherwise).
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
            // If we ENABLE advanced color for a 10-bit client, trust it (the driver will compose FP16) and
            // size the ring FP16 directly — don't race the advanced_color_enabled poll, which may not have
            // settled within 250 ms and would size the ring SDR while the driver composes FP16 → a format
            // mismatch → an immediate ring recreate + dropped first frames (audit §5.4).
            let enabled_hdr =
                want_hdr && pf_win_display::win_display::set_advanced_color(ccd, true);
            if enabled_hdr {
                // Let the colorspace change settle before the driver composes + we size the ring:
                // poll the CCD advanced-color state instead of a fixed sleep (latency plan P0.4),
                // ceiling = the old 250 ms. A read that never flips within the ceiling proceeds
                // exactly like the fixed sleep did — the ring is sized FP16 from `enabled_hdr`
                // either way (the set succeeded; only the driver's compose flip may lag, which the
                // stash/format-guard machinery absorbs).
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
            // Downgrade point D (design/hdr-10bit-default-and-av1.md item 2d): the session was
            // NEGOTIATED 10-bit (the client was told HDR in the Welcome), but the virtual display
            // could not enable advanced color — the ring sizes SDR and the encoder will emit 8-bit
            // BT.709, so the client's label overstates the stream until the descriptor poller sees
            // HDR come on. Loud, because every frame of this session is affected.
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
            // Our device (ring + zero-copy NVENC) lives on `luid` — the selected render GPU per
            // `open_inner`; the driver must render the swap-chain on the SAME adapter for the
            // shared textures to open (it reports its actual render LUID into the header, which
            // `open_inner` uses to rebind once if this mismatches).
            let factory: IDXGIFactory4 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapterByLuid(luid)
                .context("EnumAdapterByLuid(render adapter) for IDD push")?;
            let (device, context) = make_device(&adapter).context("make_device for IDD push")?;

            let sa = SharedObjectSa::new()?;
            let bytes = std::mem::size_of::<SharedHeader>().max(64);

            // Header — UNNAMED (the sealed channel: the driver gets a duplicated handle, not a name).
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(sa.as_ptr()),
                PAGE_READWRITE,
                0,
                bytes as u32,
                PCWSTR::null(),
            )
            .context("CreateFileMapping(IDD-push header)")?;
            // Own the mapping handle so it (and its view) free via `MappedSection` RAII even on bail.
            let map = OwnedHandle::from_raw_handle(map.0 as _);
            let view = MapViewOfFile(
                HANDLE(map.as_raw_handle()),
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                bytes,
            );
            if view.Value.is_null() {
                bail!("MapViewOfFile failed for IDD-push header"); // `map` drops → mapping closed
            }
            let section = MappedSection { handle: map, view };
            let generation = next_generation();
            let header = section.ptr::<SharedHeader>();
            std::ptr::write_bytes(header.cast::<u8>(), 0, bytes);
            (*header).version = VERSION;
            (*header).generation = generation;
            (*header).ring_len = RING_LEN;
            (*header).width = w;
            (*header).height = h;
            // Ring format = the display's composition format (FP16 in HDR, BGRA in SDR). The driver
            // reads this into its `ring_format` and drops any surface that doesn't match.
            (*header).dxgi_format = ring_fmt.0 as u32;
            // The ring NAMES its monitor (proto v3, `design/idd-push-security.md` invariant #10) —
            // stamped before the magic (below), never changed for the ring's life (a mid-session
            // recreate reuses this mapping). The driver refuses to attach a ring naming a different
            // monitor, so a stash cross-wire fails closed instead of leaking frames cross-client
            // (fail-closed refusal VALIDATED on-glass 2026-07-10 via a fault-injected build: driver
            // DRV_STATUS_BIND_FAIL + loud host open failure + sibling stream undisturbed).
            (*header).target_id = target.target_id;

            // Frame-ready event (auto-reset) — UNNAMED, like everything on this channel.
            let event = CreateEventW(Some(sa.as_ptr()), false, false, PCWSTR::null())
                .context("CreateEvent(IDD-push)")?;
            let event = OwnedHandle::from_raw_handle(event.0 as _);

            // Ring of shared keyed-mutex textures, format matched to the display's current mode.
            let slots = Self::create_ring_slots(&device, w, h, ring_fmt)?;

            // Publish: magic LAST (Release) — the ring must be fully initialized before the driver
            // (which receives the channel strictly afterwards) can observe MAGIC.
            std::sync::atomic::fence(Ordering::Release);
            (*(std::ptr::addr_of!((*header).magic) as *const AtomicU32))
                .store(MAGIC, Ordering::Release);

            // Deliver the sealed channel: duplicate header + event + every slot texture into the
            // driver's WUDFHost and hand it the values over the control device. All-or-nothing (the
            // broker reaps its remote duplicates on failure), and a failure fails the open — without
            // the delivery the driver can never attach.
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

            // v5 hardware-cursor channel (M2c): create + deliver the CursorShm section. Failure
            // is NON-fatal — the driver never declares the hardware cursor without this delivery,
            // so the session degrades to today's composited pointer (and the forwarder simply
            // never sees a live overlay).
            let cursor_shared = cursor_sender.as_ref().and_then(|send_cursor| {
                match cursor::CursorShared::create(ccd) {
                    Ok(cs) => {
                        // Deliver via the shared helper (also used for RE-delivery after a
                        // driver-side monitor re-arrival destroyed the worker).
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
            // No LIVE channel this session, but the target's sticky declare (an EARLIER session's —
            // irrevocable, §8.6) keeps DWM's frames pointer-free with no client drawing either:
            // the only visible pointer is the one composited here, so force composite mode on.
            //
            // Gated on `cursor_shared`, NOT on `cursor_sender`. §8.6's rationale is "this session has
            // no cursor CHANNEL", and the delivery just above is explicitly allowed to fail
            // non-fatally — which is precisely the state that needs this rescue, yet the
            // `cursor_sender.is_none()` test was the one state that skipped it: the host negotiated a
            // channel, failed to create or deliver it, and then declined to composite, leaving a
            // cursor-excluded target with NO pointer at all.
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
            // The GDI shape poller rides the SAME gate as the delivered channel: with the driver's
            // hardware cursor keeping the frame cursor-free, the poller supplies the full-fidelity
            // shape (masked/monochrome included — the IddCx query can't; see cursor_poll.rs).
            // Forced-composite sessions need it too — it is their only shape/position source.
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
            // Heal the driver's persisted cursor-forward state: a session that died on the
            // secure desktop (client drops at the lock screen — the common case) leaves the
            // per-target desired state `false`, and the NEXT session's channel delivery would
            // adopt UNdeclared (the exact cross-session composite trap of §8.6). A fresh
            // session always starts declared; the secure-desktop guard re-disables if the
            // secure desktop is (still) up, via its first `poll_secure_desktop` edge.
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
                // Whether DXGI ever reached the win32u GPU-preference hook. By this point the
                // factory + `EnumAdapterByLuid` + `make_device` above have exercised DXGI, so a
                // 0 here means the hook is inert on this build — the first thing to check if a
                // hybrid-GPU box keeps reporting TEX_FAIL render-adapter mismatches
                // (`dxgi::install_gpu_pref_hook`).
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
                stale_trips: 0,
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
                // Held from BEFORE the first-frame gate (the display must not idle off while we
                // wait for the first compose) until the capturer drops with the session.
                _display_wake: pf_frame::session_tuning::DisplayWakeRequest::new(),
                // Placeholder; `open()` attaches the real keepalive on success, so a FAILED open can hand
                // it back to the caller to retire or reuse the display (audit §5.1).
                _keepalive: Box::new(()),
            };
            // The two REALTIME GPU-priority opt-ins, stamped once per capture session so EVERY
            // field log self-describes its posture — the stall WARNs repeat them, but only when
            // they fire, and the 7700 XT case (2026-08-26) showed a stalling log where they
            // never did (design: windows-amd-host-program §3.1 Gap B).
            tracing::info!(
                rt_gpu_driver = super::stall::rt_gpu_driver_posture(),
                rt_gpu_host = super::stall::rt_gpu_host_posture(),
                "GPU-priority posture for this capture session"
            );
            // The HDR SDR-white reference for the composited cursor, queried ONCE here rather than
            // from the blend (which holds the ring slot's keyed mutex — see
            // `refresh_sdr_white_scale`). No-op on an SDR composition.
            me.refresh_sdr_white_scale();
            // Bounded wait for the driver to ATTACH to the ring AND publish a first frame. An attach
            // failure (DRV_STATUS_TEX_FAIL) or an attach-but-no-frames (a game left the display in a
            // format/size the ring can't match) becomes an open failure the caller handles by retiring the
            // display, instead of next_frame's 20 s black-then-bail.
            me.wait_for_attach()?;
            Ok(me)
        }
    }

    /// Block (bounded) until the driver has ATTACHED to the host ring (`DRV_STATUS_OPENED`) **and published
    /// a first frame**, else fail so the caller can retire the display and rebuild (audit §5.1 +
    /// `design/windows-host-rewrite.md` §2.5 — the GB1 game-capture fix).
    ///
    /// Requiring the first frame — not just the attach — catches the *reconnect-into-a-broken-state* case:
    /// a fullscreen game can leave the virtual display in a format/size that the driver's `publish()` guard
    /// rejects, so the driver ATTACHES but silently drops every frame; without this the host sails past
    /// `open()` and only dies on `next_frame`'s 20 s deadline (the "reconnect = black + audio" symptom).
    /// A stash-capable driver republishes its retained desktop frame the moment it attaches (the
    /// first-frame guarantee — `FrameStash`, driver frame_transport.rs), so the normal case clears this
    /// gate in milliseconds even on an idle desktop; failing that, at session open the OS activates the
    /// virtual display → DWM composites it → a frame arrives within ~1 s, plus the compose-kick fallback
    /// below — no frame within the window = genuinely broken.
    fn wait_for_attach(&self) -> Result<()> {
        // Symmetric host-side binding sanity (proto v3 §3.2): OUR header must still name OUR
        // monitor. The stamp is ours and nothing legitimate rewrites it, so a mismatch means a
        // host-side bug (a stash/capturer cross-wire) — the exact class the driver-side check
        // catches from the other end; failing here names the culprit in the same release.
        // SAFETY: in-bounds, aligned u32 read of the live, owned shared-header mapping (same access
        // pattern as the `driver_status` read below); no reference into the shared region is formed.
        let stamped = unsafe { (*self.header).target_id };
        if stamped != self.target_id {
            bail!(
                "IDD-push: our ring header names target {stamped} but this capturer serves target \
                 {} — host-side ring↔monitor cross-wire (bug); failing the open",
                self.target_id
            );
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        // First-frame expectation: a stash-capable driver republishes its retained desktop frame
        // the moment it attaches (`FrameStash`, frame_transport.rs), so on a healthy pairing the
        // gate below clears in milliseconds even on a perfectly idle desktop. The compose-kick
        // schedule is the FALLBACK for pre-stash drivers / an empty stash (a display that has
        // never composed): DWM only presents a display something DIRTIED, so on an idle desktop
        // an attach would otherwise sit at E_PENDING forever and fail this gate — the
        // "idle desktop → no frames" gotcha. Give the natural post-activate compose (and the
        // stash republish) a moment, then nudge; log when we do, so field logs show whether the
        // stash path is working.
        let mut next_kick = Instant::now() + Duration::from_millis(600);
        loop {
            // SAFETY: `self.header` points into the live shared-header mapping this capturer owns (sized
            // `>= size_of::<SharedHeader>()`, page-aligned), so the field read is in-bounds + aligned, and
            // no reference into the shared region is formed. Plain read: the driver writes this `u32`
            // cross-process, but an aligned `u32` read can't tear and `driver_status` is best-effort
            // diagnostics — the real handshake is the atomic `magic`/`latest` (same access as
            // log_driver_status_once).
            let st = unsafe { (*self.header).driver_status };
            if st == DRV_STATUS_TEX_FAIL {
                // The driver wrote its render LUID BEFORE attempting the texture opens
                // (frame_transport.rs step 2), so it is valid here.
                let (_, detail, lo, hi) = self.driver_diag();
                // Typed so `open_inner` can rebind the ring to the driver's adapter once.
                return Err(anyhow::Error::new(AttachTexFail {
                    detail,
                    driver_luid: ((hi as i64) << 32) | (lo as i64 & 0xffff_ffff),
                }));
            }
            if st == DRV_STATUS_NO_DEVICE1 {
                // SAFETY: as above — an in-bounds, aligned `u32` read of a best-effort diagnostic field
                // through the owned, live header mapping; no reference into the shared region is formed.
                let detail = unsafe { (*self.header).driver_status_detail };
                bail!(
                    "IDD-push driver failed to attach (driver_status={st} detail=0x{detail:08x} — \
                     the driver has no ID3D11Device1 to open shared resources)"
                );
            }
            if st == DRV_STATUS_BIND_FAIL {
                // SAFETY: as above — an in-bounds, aligned `u32` read of a best-effort diagnostic field
                // through the owned, live header mapping; no reference into the shared region is formed.
                let claimed = unsafe { (*self.header).driver_status_detail };
                bail!(
                    "IDD-push driver REFUSED the ring↔monitor binding (DRV_STATUS_BIND_FAIL: the \
                     delivered ring names target {claimed}, the monitor is {}) — host \
                     stash/delivery cross-wire (bug); failing the open loudly (proto v3 §3.2)",
                    self.target_id
                );
            }
            // Attached AND a frame has been published — the publish token's seq advances past 0.
            if st == DRV_STATUS_OPENED && frame::FrameToken::unpack(self.latest()).seq != 0 {
                return Ok(());
            }
            if Instant::now() >= next_kick {
                // Reaching a kick at all means the driver did NOT republish a retained frame
                // (pre-stash driver, or a never-composed display) — worth a line in the field log.
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
            // Event-driven wait (latency plan P0.6): the driver signals the frame-ready event on
            // every publish, so wake on it instead of a blind sleep — the 20 ms timeout keeps the
            // driver_status polls above live (status writes don't signal the event). Consuming a
            // signal here is fine: `next_frame` re-checks the atomic `latest` token, never the
            // event, for truth.
            // SAFETY: `self.event` is this capturer's owned, live auto-reset event handle;
            // `WaitForSingleObject` only reads the handle and the 20 ms timeout bounds the wait.
            let _ = unsafe { WaitForSingleObject(HANDLE(self.event.as_raw_handle()), 20) };
        }
    }

    /// Name a first-frame timeout from the driver's own evidence — `driver_status` plus the live
    /// OPENED detail word (proto `pack_opened_detail`) — instead of guessing. The three no-frames
    /// states look identical from the host side but have disjoint causes and fixes; the lid-closed
    /// field report burned days for lack of exactly this line. Appends a console-session hint when
    /// the host itself is in the wrong session (display writes + input kicks can't work from there).
    fn no_first_frame_diagnosis(&self, st: u32) -> String {
        let what = match st {
            // The delivery was never consumed: no swap-chain worker ran for this monitor at all.
            DRV_STATUS_NONE => "the driver never attached — the channel delivery was never \
                 consumed, so the OS ran no swap-chain worker for this monitor (display not \
                 composed at all: console display-off / modern standby, or the mode commit \
                 never reached the adapter)"
                .to_string(),
            DRV_STATUS_OPENED => {
                // SAFETY: in-bounds, aligned u32 read of the live, owned shared-header mapping
                // (same best-effort diagnostic access as the `driver_status` read in the caller);
                // no reference into the shared region is formed.
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
                    // A pre-detail driver never stamps the live bit — say so rather than guess.
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

/// `wait_for_attach`'s DRV_STATUS_TEX_FAIL as a typed error: the driver could not open the ring
/// textures, and `driver_luid` (packed, from the shared header) is the adapter its swap-chain
/// ACTUALLY renders on — `open_inner` downcasts to this to rebind the ring there once.
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
