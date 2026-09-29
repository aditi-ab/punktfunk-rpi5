//! Pads and pointer settings, and what the host sends back to the pads: controller
//! mouse, scroll direction, live pads, rumble and HID output.

#[cfg(feature = "quic")]
use crate::*;

/// Switch the pads in `mask` (bit = wire pad index) to controller mouse: their buttons and sticks
/// drive the host pointer and a few keys while the host pad sits neutral. `0` returns every pad
/// to passthrough. Session-scoped. `Unsupported` without `PUNKTFUNK_GRANT_POINTER`.
///
/// # Safety
/// `c` is a valid connection handle. Callable from any thread.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_pad_mouse(
    c: *mut PunktfunkConnection,
    mask: u16,
) -> PunktfunkStatus {
    with_conn!(c => {
        status_of(c.inner.set_pad_mouse(mask))
    })
}

/// Change scroll direction for this session at the shared outbound seam.
///
/// # Safety
/// `c` is a valid connection handle. Callable from any thread.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_invert_scroll(
    c: *mut PunktfunkConnection,
    invert: bool,
) -> PunktfunkStatus {
    with_conn!(c => {
        c.inner.set_invert_scroll(invert);
        PunktfunkStatus::Ok
    })
}

/// Pads in controller mouse now. A removed pad or a lost pointer grant clears its bit.
///
/// # Safety
/// `c` is a valid connection handle; `mask` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_pad_mouse(
    c: *const PunktfunkConnection,
    mask: *mut u16,
) -> PunktfunkStatus {
    conn_out!(c, mask => c.inner.pad_mouse())
}

/// Wire pad indices the host holds now, a bit per pad: declared or driven, not yet removed.
///
/// # Safety
/// `c` is a valid connection handle; `mask` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_live_pads(
    c: *const PunktfunkConnection,
    mask: *mut u16,
) -> PunktfunkStatus {
    conn_out!(c, mask => c.inner.live_pads())
}

/// Pull the next rumble update, waiting up to `timeout_ms`. Amplitudes are
/// 0..0xFFFF (`low`/`high` motors), `(0, 0)` = stop. Same timeout/closed as
/// [`punktfunk_connection_next_audio`]. Drops the v2 self-terminating TTL —
/// use [`punktfunk_connection_next_rumble2`] for the host-supplied lease.
///
/// # Safety
/// `c` is a valid connection handle; out pointers are writable (NULLs skipped).
/// At most one rumble puller; it may run concurrently with video/audio.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_rumble(
    c: *mut PunktfunkConnection,
    pad: *mut u16,
    low: *mut u16,
    high: *mut u16,
    timeout_ms: u32,
) -> PunktfunkStatus {
    // SAFETY: pointers forwarded unchanged; `next_rumble2` skips a null `ttl_ms`.
    unsafe {
        punktfunk_connection_next_rumble2(c, pad, low, high, std::ptr::null_mut(), timeout_ms)
    }
}

/// `*ttl_ms` sentinel from [`punktfunk_connection_next_rumble2`] when the host sent
/// no self-termination lease. Fall back to a client-side staleness heuristic.
pub const PUNKTFUNK_RUMBLE_NO_TTL: u32 = 0xFFFF_FFFF;

/// Pull the next rumble update including its self-termination TTL. Same
/// `pad`/`low`/`high` as [`punktfunk_connection_next_rumble`], plus `*ttl_ms`:
/// milliseconds to render this level unless the host renews. [`PUNKTFUNK_RUMBLE_NO_TTL`]
/// = no lease; fall back to a client-side timeout. Reorder gate is applied inside.
///
/// # Safety
/// `c` is a valid connection handle; out pointers are writable (NULLs skipped).
/// At most one rumble puller; it may run concurrently with video/audio.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_rumble2(
    c: *mut PunktfunkConnection,
    pad: *mut u16,
    low: *mut u16,
    high: *mut u16,
    ttl_ms: *mut u32,
    timeout_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        match c
            .inner
            .next_rumble_ttl(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok((p, l, h, ttl)) => {
                // SAFETY: the caller passes each out-param null or writable for one value.
                unsafe {
                    put(pad, p);
                    put(low, l);
                    put(high, h);
                    put(ttl_ms, ttl.map_or(PUNKTFUNK_RUMBLE_NO_TTL, u32::from));
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// `flags` bit for [`punktfunk_connection_set_rumble_quirks`]: alternate the low
/// motor's LSB on keepalive re-emits so an SDL-class layer that no-ops identical
/// values still writes the device.
pub const PUNKTFUNK_RUMBLE_QUIRK_DEDUP_JITTER: u32 = 1;

/// Effective rumble from the shared policy engine. No TTL: apply `(0, 0)` as
/// stop, else run at this level; `*backstop_ms` is a safety-net duration (`0` on
/// stop). Handle motors only — triggers are [`punktfunk_connection_next_rumble_cmd2`].
/// Mutually exclusive with `next_rumble`/`next_rumble2`.
///
/// # Safety
/// `c` is a valid connection handle; out pointers are writable (NULLs skipped).
/// At most one rumble puller; it may run concurrently with video/audio.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_rumble_cmd(
    c: *mut PunktfunkConnection,
    pad: *mut u16,
    low: *mut u16,
    high: *mut u16,
    backstop_ms: *mut u32,
    timeout_ms: u32,
) -> PunktfunkStatus {
    // SAFETY: pointers forwarded unchanged; `next_rumble_cmd2` skips null trigger out-params.
    unsafe {
        punktfunk_connection_next_rumble_cmd2(
            c,
            pad,
            low,
            high,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            backstop_ms,
            timeout_ms,
        )
    }
}

/// [`punktfunk_connection_next_rumble_cmd`] plus Xbox impulse-trigger motors.
/// New symbol — growing the old signature would stack-corrupt old embedders.
/// Render triggers only on pads that have them; never fold into handles.
/// Same plane as `next_rumble_cmd`; call exactly one.
///
/// # Safety
/// `c` is a valid connection handle; out pointers are writable (NULLs skipped).
/// At most one rumble puller; it may run concurrently with video/audio.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_rumble_cmd2(
    c: *mut PunktfunkConnection,
    pad: *mut u16,
    low: *mut u16,
    high: *mut u16,
    left_trigger: *mut u16,
    right_trigger: *mut u16,
    backstop_ms: *mut u32,
    timeout_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        match c
            .inner
            .next_rumble_command(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(cmd) => {
                // SAFETY: the caller passes each out-param null or writable for one value.
                unsafe {
                    put(pad, cmd.pad);
                    put(low, cmd.low);
                    put(high, cmd.high);
                    put(left_trigger, cmd.left_trigger);
                    put(right_trigger, cmd.right_trigger);
                    put(backstop_ms, cmd.backstop_ms);
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Per-pad rumble quirks (call at attach). `keepalive_ms`: re-emit non-zero
/// (Steam Deck ≈ 40); `0` = none. `min_pulse_ms`: floor for `backstop_ms`.
/// A renderer that dedupes its own writes cannot use `keepalive_ms`.
///
/// # Safety
/// `c` is a valid connection handle. Callable from any thread.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_rumble_quirks(
    c: *mut PunktfunkConnection,
    pad: u16,
    keepalive_ms: u16,
    min_pulse_ms: u16,
    flags: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        c.inner.set_rumble_quirks(
            pad,
            punktfunk_core::client::ActuatorQuirks {
                keepalive_ms,
                min_pulse_ms,
                dedup_jitter: flags & PUNKTFUNK_RUMBLE_QUIRK_DEDUP_JITTER != 0,
            },
        );
        PunktfunkStatus::Ok
    })
}

/// Pull the next HID-output feedback (DualSense lightbar / player LEDs / adaptive
/// trigger, or SC2 `PUNKTFUNK_HIDOUT_HID_RAW`) into `*out`.
/// [`PunktfunkStatus::NoFrame`] on timeout, [`PunktfunkStatus::Closed`] once ended.
/// DualSense and SC2 backends only. One puller, may run alongside other planes.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkHidOutput`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_hidout(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkHidOutput,
    timeout_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_hidout(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(h) => {
                // SAFETY: `out` is non-null on this path; written once by value.
                unsafe { *out = PunktfunkHidOutput::from_hid(&h) };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}
