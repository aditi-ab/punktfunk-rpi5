//! The management API over the stream port, for a shell that cannot dial TCP 47990.

#[cfg(feature = "quic")]
use crate::*;

/// One management request over the stream port, as this device's identity, refusing any host
/// but `pin_sha256`: `method` `path` with `body`, sent as `content_type` unless that is NULL.
/// Blocks for the reply. One connection per host serves every call; a new one is dialled for at
/// most `timeout_ms`. On `OK` the reply's HTTP status is in `status_out` and its body in
/// `body_out`/`body_len_out`; free that with [`punktfunk_mgmt_body_free`]. `CRYPTO`: the host is
/// not the pinned one, so do not retry it over HTTPS. Any other failure: the host may still
/// answer on its management port.
///
/// # Safety
/// `host`/`client_cert_pem`/`client_key_pem`/`method`/`path` are NUL-terminated UTF-8, and so
/// is `content_type` unless NULL; `pin_sha256` is valid for 32 bytes; `body` is NULL or valid
/// for `body_len` bytes; the three out-pointers are writable.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_mgmt_request(
    host: *const std::os::raw::c_char,
    port: u16,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    method: *const std::os::raw::c_char,
    path: *const std::os::raw::c_char,
    content_type: *const std::os::raw::c_char,
    body: *const u8,
    body_len: usize,
    timeout_ms: u32,
    status_out: *mut u16,
    body_out: *mut *mut u8,
    body_len_out: *mut usize,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: each pointer is caller-supplied and null-checked by `opt_cstr`.
        let strs = unsafe {
            (
                opt_cstr(host),
                opt_cstr(client_cert_pem),
                opt_cstr(client_key_pem),
                opt_cstr(method),
                opt_cstr(path),
                opt_cstr(content_type),
            )
        };
        let (
            Ok(Some(host)),
            Ok(Some(cert)),
            Ok(Some(key)),
            Ok(Some(method)),
            Ok(Some(path)),
            Ok(content_type),
        ) = strs
        else {
            return PunktfunkStatus::NullPointer;
        };
        if pin_sha256.is_null() || status_out.is_null() || body_out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        if body_len_out.is_null() || (body.is_null() && body_len > 0) {
            return PunktfunkStatus::NullPointer;
        }
        let mut pin = [0u8; 32];
        // SAFETY: `pin_sha256` is non-null and valid for 32 bytes (caller contract).
        pin.copy_from_slice(unsafe { std::slice::from_raw_parts(pin_sha256, 32) });
        let sent: &[u8] = match body_len {
            0 => &[],
            // SAFETY: `body` is non-null here and valid for `body_len` bytes.
            n => unsafe { std::slice::from_raw_parts(body, n) },
        };
        let wait = std::time::Duration::from_millis(u64::from(timeout_ms));
        let req = punktfunk_core::client::mgmt::Request {
            method,
            path,
            content_type,
            body: sent,
        };
        let reply =
            punktfunk_core::client::mgmt::request(host, port, (cert, key), Some(pin), wait, &req);
        let reply = match reply {
            Ok(r) => r,
            Err(e) => return e.status(),
        };
        let (ptr, len) = match reply.body.len() {
            0 => (std::ptr::null_mut(), 0),
            n => (Box::into_raw(reply.body.into_boxed_slice()).cast::<u8>(), n),
        };
        // SAFETY: the three out-pointers are non-null and writable (checked above).
        unsafe {
            put(status_out, reply.status);
            put(body_out, ptr);
            put(body_len_out, len);
        }
        PunktfunkStatus::Ok
    })
}

/// Free a reply body from [`punktfunk_mgmt_request`]. NULL is a no-op.
///
/// # Safety
/// `body`/`len` are exactly what one `punktfunk_mgmt_request` wrote, freed once.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_mgmt_body_free(body: *mut u8, len: usize) {
    if body.is_null() {
        return;
    }
    // SAFETY: `body`/`len` came from `Box::<[u8]>::into_raw` in `punktfunk_mgmt_request`.
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(body, len)) });
}

#[cfg(all(test, feature = "quic"))]
mod tests {
    use super::*;

    /// A null argument is refused before anything dials.
    #[test]
    fn a_missing_argument_dials_nothing() {
        let (mut status, mut body, mut len) = (0u16, std::ptr::null_mut(), 0usize);
        let s = c"x";
        // SAFETY: every non-null pointer is a live C string or a writable local.
        let got = unsafe {
            punktfunk_mgmt_request(
                s.as_ptr(),
                9777,
                s.as_ptr(),
                s.as_ptr(),
                std::ptr::null(),
                s.as_ptr(),
                s.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                1,
                &mut status,
                &mut body,
                &mut len,
            )
        };
        assert_eq!(got, PunktfunkStatus::NullPointer);
        assert!(body.is_null());
        // SAFETY: NULL is the documented no-op.
        unsafe { punktfunk_mgmt_body_free(std::ptr::null_mut(), 0) };
    }
}
