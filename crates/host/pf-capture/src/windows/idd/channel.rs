//! The host's channel to the driver: the WUDFHost handle-duplication broker, and
//! [`IddTransport`], the remote source's edge into the driver.
//!
//! The AU and cursor sections are unnamed. The driver reaches them only through
//! handles this broker duplicates into its WUDFHost, delivered as bare values by
//! the owning IOCTL. The process handle doubles as the driver-death probe.
//! Evidence: `design/idd-push-security.md`.

use super::*;
use crate::windows::remote::broker::RemoteProcess;
use crate::windows::remote::transport::{HandleTarget, RemoteEndpoint, Transport, TransportEvent};
use crate::windows::tick::driver_probe_due;
use crate::{EncodeCtlSender, EncoderSite, SetEncodeSender};
use pf_driver_proto::encode::{SetEncodeReply, SetEncodeRequest};

/// On IOCTL success the driver owns (and closes) the duplicates; on any failure the
/// deliverer reaps what it planted ([`Self::close_remote_public`]).
pub(super) struct ChannelBroker {
    /// WUDFHost process (`ProcessSharingDisabled` — exclusively pf-vdisplay's).
    /// `SYNCHRONIZE` doubles as the driver-death probe ([`Self::driver_alive`]).
    process: RemoteProcess,
    pub(super) wudf_pid: u32,
}

impl ChannelBroker {
    /// Open the WUDFHost duplication target. `wudf_pid == 0` is a driver that
    /// predates the sealed channel. [`open_wudfhost`] proves the image path before
    /// any section handle is duplicated into it: a spoofed ADD pid (same interface
    /// GUID, different process) would otherwise receive the stream.
    pub(super) fn open(wudf_pid: u32) -> Result<Self> {
        let process = open_wudfhost(wudf_pid, "driver-channel")?;
        Ok(Self {
            process: RemoteProcess::new(process, "the driver's WUDFHost"),
            wudf_pid,
        })
    }

    /// Raise WUDFHost's GPU scheduling class. The driver's encoder submits its GPU work
    /// there, so the host's own class covers none of it. Best-effort. `self.process` keeps
    /// the verified WUDFHost alive, so `wudf_pid` cannot name another process meanwhile.
    pub(super) fn raise_gpu_priority(&self) {
        // SAFETY: plain open by pid; the result is checked before use.
        let h = match unsafe { OpenProcess(PROCESS_SET_INFORMATION, false, self.wudf_pid) } {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(wudf_pid = self.wudf_pid, error = %e, "WUDFHost GPU priority not raised");
                return;
            }
        };
        // SAFETY: `h` was just opened here; `OwnedHandle` becomes its sole owner.
        let owned = unsafe { OwnedHandle::from_raw_handle(h.0 as _) };
        // SAFETY: `owned` is live for the call and carries PROCESS_SET_INFORMATION.
        unsafe {
            pf_frame::dxgi::elevate_gpu_priority_of(HANDLE(owned.as_raw_handle()), "WUDFHost")
        };
    }

    /// Signaled ⇔ WUDFHost exited. A dead driver and an idle desktop both just stop
    /// advancing the source counter, so this is the only death signal.
    pub(super) fn driver_alive(&self) -> bool {
        !self.process.exited()
    }

    /// Duplicate a cursor section into WUDFHost with the same `SECTION_MAP_RW` as the
    /// AU section.
    pub(super) fn dup_into_public(&self, h: BorrowedHandle<'_>) -> Result<u64> {
        self.process.dup_into(h, Some(SECTION_MAP_RW))
    }

    /// Failure-path reaper for a cursor-channel duplicate the driver never adopted.
    pub(super) fn close_remote_public(&self, value: u64) {
        self.process.close_remote(value);
    }
}

/// The driver's side of the remote source: its WUDFHost, and the two IOCTLs the host sends.
pub(super) struct IddTransport {
    broker: Arc<ChannelBroker>,
    target_id: u32,
    /// The display's CCD key, as the log lines name it.
    name: String,
    set_encode: SetEncodeSender,
    encode_ctl: EncodeCtlSender,
    /// The last driver-death probe.
    probed: Instant,
}

impl IddTransport {
    pub(super) fn new(
        broker: Arc<ChannelBroker>,
        ccd: pf_win_display::win_display::CcdTargetKey,
        set_encode: SetEncodeSender,
        encode_ctl: EncodeCtlSender,
    ) -> Self {
        Self {
            broker,
            target_id: ccd.target_id,
            name: ccd.to_string(),
            set_encode,
            encode_ctl,
            probed: Instant::now(),
        }
    }
}

impl Transport for IddTransport {
    fn endpoint(&self) -> RemoteEndpoint {
        RemoteEndpoint {
            site: EncoderSite::Driver,
            target_id: self.target_id,
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn handles(&self) -> &dyn HandleTarget {
        &self.broker.process
    }

    /// Every open re-raises WUDFHost's GPU scheduling class: the driver's encoder submits its
    /// work there, so the host's own class covers none of it.
    fn set_encode(&self, req: &SetEncodeRequest) -> Result<SetEncodeReply> {
        self.broker.raise_gpu_priority();
        (self.set_encode)(req)
    }

    fn encode_ctl(&self) -> EncodeCtlSender {
        self.encode_ctl.clone()
    }

    fn secure_desktop(&self) -> bool {
        pf_win_display::secure_desktop()
    }

    /// Probe while stale ([`driver_probe_due`]) so the driver cycle fires instead of the session
    /// streaming nothing.
    fn poll(&mut self, now: Instant, last_fresh: Instant) -> Result<Vec<TransportEvent>> {
        if !driver_probe_due(now, last_fresh, self.probed) {
            return Ok(Vec::new());
        }
        self.probed = now;
        if self.broker.driver_alive() {
            return Ok(Vec::new());
        }
        tracing::warn!(
            wudf_pid = self.broker.wudf_pid,
            "IDD push: the pf-vdisplay WUDFHost is gone — firing the driver cycle"
        );
        Ok(vec![TransportEvent::DriverGone])
    }
}
