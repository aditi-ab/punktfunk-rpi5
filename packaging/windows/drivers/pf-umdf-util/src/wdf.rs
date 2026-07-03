//! Safe(ly-contracted) helpers over the WDF request/memory/property DDIs the pad drivers use. The
//! pattern: a framework callback converts its raw `WDFREQUEST` into a [`Request`] token **once**
//! (`unsafe`, the framework's validity guarantee is the contract); every operation after that is a
//! safe method, and completion consumes the token so a request cannot be completed twice or used
//! after completion from safe code.

use wdk_sys::{
    NTSTATUS, WDF_NO_OBJECT_ATTRIBUTES, WDFDEVICE, WDFMEMORY, WDFQUEUE, WDFREQUEST,
    call_unsafe_wdf_function_binding,
};

const STATUS_INVALID_BUFFER_SIZE: NTSTATUS = 0xC000_0206u32 as NTSTATUS;
/// DEVICE_REGISTRY_PROPERTY: DevicePropertyLocationInformation (the const isn't re-exported at the
/// wdk_sys root; the value is stable WDM).
const DEVICE_PROPERTY_LOCATION_INFORMATION: i32 = 10;

#[inline]
fn nt_success(s: NTSTATUS) -> bool {
    s >= 0
}

/// A validity token for one framework-delivered `WDFREQUEST`. Not `Copy`/`Clone`: completing or
/// forwarding consumes it, so safe code cannot touch a request the framework already owns again.
pub struct Request(WDFREQUEST);

impl Request {
    /// Wrap the raw request handed to the current framework callback.
    ///
    /// # Safety
    /// `raw` must be the live, framework-provided `WDFREQUEST` of the callback invocation this is
    /// called from (WDF owns handle validity; a forged/dangling handle is framework UB).
    pub unsafe fn new(raw: WDFREQUEST) -> Request {
        Request(raw)
    }

    /// Complete the request with `status` (consumes the token — the framework owns it afterwards).
    pub fn complete(self, status: NTSTATUS) {
        // SAFETY: `self.0` is the live callback request per `Request::new`'s contract, not yet
        // completed or forwarded (both consume the token).
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, self.0, status) };
    }

    /// Copy `src` into the request's (buffered) output buffer and set the completed byte count.
    /// Returns the status to complete with (`STATUS_INVALID_BUFFER_SIZE` if the buffer is short).
    pub fn copy_to_output(&self, src: &[u8]) -> NTSTATUS {
        let mut mem: WDFMEMORY = core::ptr::null_mut();
        // SAFETY: `self.0` is the live callback request; `mem` receives the memory handle.
        let st = unsafe {
            call_unsafe_wdf_function_binding!(WdfRequestRetrieveOutputMemory, self.0, &mut mem)
        };
        if !nt_success(st) {
            return st;
        }
        let mut outlen: usize = 0;
        // SAFETY: `mem` is the valid memory object just retrieved; `outlen` receives its size.
        let _ = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, mem, &mut outlen) };
        if outlen < src.len() {
            return STATUS_INVALID_BUFFER_SIZE;
        }
        // SAFETY: `mem` is valid and at least `src.len()` bytes; `src` is a live borrow.
        let st = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCopyFromBuffer,
                mem,
                0usize,
                src.as_ptr() as *mut core::ffi::c_void,
                src.len()
            )
        };
        if !nt_success(st) {
            return st;
        }
        // SAFETY: `self.0` is the live callback request.
        unsafe {
            call_unsafe_wdf_function_binding!(WdfRequestSetInformation, self.0, src.len() as u64)
        };
        0 // STATUS_SUCCESS
    }

    /// The request's input buffer: up to `cap` bytes copied out, plus the buffer's TRUE length.
    /// `Err(status)` if the input memory can't be retrieved (propagate as the completion status).
    pub fn input_bytes(&self, cap: usize) -> Result<(Vec<u8>, usize), NTSTATUS> {
        let mut inmem: WDFMEMORY = core::ptr::null_mut();
        // SAFETY: `self.0` is the live callback request; `inmem` receives the memory handle.
        let st = unsafe {
            call_unsafe_wdf_function_binding!(WdfRequestRetrieveInputMemory, self.0, &mut inmem)
        };
        if !nt_success(st) {
            return Err(st);
        }
        let mut len: usize = 0;
        // SAFETY: `inmem` is the valid memory object just retrieved; `len` receives its size.
        let p = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, inmem, &mut len) }
            as *const u8;
        if p.is_null() {
            return Ok((Vec::new(), 0));
        }
        let n = len.min(cap);
        // SAFETY: `p` is valid for `len` bytes per `WdfMemoryGetBuffer`; we read `n <= len`.
        let bytes = unsafe { core::slice::from_raw_parts(p, n) }.to_vec();
        Ok((bytes, len))
    }

    /// The request's output-buffer LENGTH (0 if unavailable) — UMDF HID marshalling carries the
    /// output-report id in it.
    pub fn output_buffer_len(&self) -> usize {
        let mut outmem: WDFMEMORY = core::ptr::null_mut();
        // SAFETY: `self.0` is the live callback request; output memory is optional here.
        if !nt_success(unsafe {
            call_unsafe_wdf_function_binding!(WdfRequestRetrieveOutputMemory, self.0, &mut outmem)
        }) {
            return 0;
        }
        let mut outlen: usize = 0;
        // SAFETY: `outmem` is the valid memory object just retrieved; `outlen` receives its size.
        let _ =
            unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, outmem, &mut outlen) };
        outlen
    }

    /// Set the completed-bytes information field (for paths that complete with a length but no
    /// output copy, e.g. echoing an output report's length).
    pub fn set_information(&self, info: u64) {
        // SAFETY: `self.0` is the live callback request.
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestSetInformation, self.0, info) };
    }

    /// Forward the request to a manual queue. On success the framework owns it (the token is
    /// consumed by value — the caller cannot touch the request again); on failure the token is
    /// handed back with the status so the caller completes it. (`Request` has no `Drop`, so the
    /// consumed-on-success token simply falls out of scope — nothing to run.)
    ///
    /// # Safety
    /// `queue` must be a live manual `WDFQUEUE` of the same device (e.g. the one created in
    /// `EvtDeviceAdd` and stashed in a static).
    pub unsafe fn forward_to_queue(self, queue: WDFQUEUE) -> Result<(), (Request, NTSTATUS)> {
        // SAFETY: `self.0` is the live callback request; `queue` is live per this fn's contract.
        let st =
            unsafe { call_unsafe_wdf_function_binding!(WdfRequestForwardToIoQueue, self.0, queue) };
        if nt_success(st) {
            Ok(())
        } else {
            Err((self, st))
        }
    }
}

/// Pop the next pended request off a manual queue (`None` when empty).
///
/// # Safety
/// `queue` must be a live manual `WDFQUEUE` (e.g. the timer's parent object).
pub unsafe fn retrieve_next_request(queue: WDFQUEUE) -> Option<Request> {
    let mut request: WDFREQUEST = core::ptr::null_mut();
    // SAFETY: `queue` is live per this fn's contract; `request` receives the next pended request.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(WdfIoQueueRetrieveNextRequest, queue, &mut request)
    };
    // SAFETY: on success `request` is a live framework request this caller now services — the
    // exact contract `Request::new` requires.
    nt_success(st).then(|| unsafe { Request::new(request) })
}

/// Read the pad index the host stamped into the device Location (`pszDeviceLocation`), a
/// NUL-terminated UTF-16 decimal string. Defaults to 0 (single-pad) if absent. (The WDFMEMORY is
/// device-parented and freed by the framework at device teardown — one small alloc per device add.)
///
/// # Safety
/// `device` must be the live `WDFDEVICE` created in the current `EvtDeviceAdd`.
pub unsafe fn query_location_index(device: WDFDEVICE) -> u32 {
    let mut mem: wdk_sys::WDFMEMORY = core::ptr::null_mut();
    // SAFETY: `device` is live per this fn's contract; property = LocationInformation; pool ignored
    // in UMDF; `mem` receives the handle.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceAllocAndQueryProperty,
            device,
            DEVICE_PROPERTY_LOCATION_INFORMATION,
            0,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut mem
        )
    };
    if !nt_success(st) || mem.is_null() {
        return 0;
    }
    let mut len: usize = 0;
    // SAFETY: `mem` is the valid memory object just allocated; `len` receives its size.
    let buf = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, mem, &mut len) }
        as *const u16;
    if buf.is_null() {
        return 0;
    }
    let units = (len / 2).min(8);
    // SAFETY: `buf` is valid for `len` bytes per `WdfMemoryGetBuffer`; we read `units * 2 <= len`.
    let chars = unsafe { core::slice::from_raw_parts(buf, units) };
    let mut idx: u32 = 0;
    let mut any = false;
    for &c in chars {
        if c == 0 {
            break;
        }
        if (0x30..=0x39).contains(&c) {
            idx = idx.wrapping_mul(10).wrapping_add((c - 0x30) as u32);
            any = true;
        }
    }
    if any { idx } else { 0 }
}
