//! What the HID minidrivers could and could not use to answer the **channel proof**
//! (`pf_driver_proto::gamepad::ChannelProof`) — the measured record, so nobody re-derives it.
//!
//! The proof tells the host which process to duplicate a pad's DATA section into. It cannot come
//! from the bootstrap mailbox: that must be openable by LocalService (the driver's own WUDFHost runs
//! as LocalService), so any LocalService principal could name a process of its own and be handed the
//! pad's live input surface (security-review 2026-07-28). Only the driver actually bound to the
//! devnode the host created can answer that devnode's I/O — hence "ask the device stack".
//!
//! ## Measured on .173 (Win11 26200), 2026-07-28 — three attempts, one survivor
//!
//! * ❌ **`IOCTL_HID_GET_INDEXED_STRING`** (`HidD_GetIndexedString`). Fails for EVERY index — 1, 2,
//!   4, 0x0E, 0x5046 — on the same zero-access handle where the NAMED string calls succeed and
//!   return the driver's own text. hidclass does not carry an arbitrary indexed-string request to a
//!   UMDF HID minidriver at all.
//! * ❌ **A private device interface** (`WdfDeviceCreateDeviceInterface` + a private IOCTL, the shape
//!   `pf_xusb` uses). The interface REGISTERS and enumerates fine —
//!   `\\?\SWD#punktfunk#pf_mouse_0#{32244793-…}` was present — but `CreateFile` on it is refused:
//!   `ERROR_GEN_FAILURE` (31) for access 0 and GENERIC_READ, `ERROR_FILE_NOT_FOUND` (2) for
//!   GENERIC_READ|WRITE. hidclass owns `IRP_MJ_CREATE` on a devnode it is the FDO for, so a second
//!   interface on that devnode cannot be opened. This is why `pf_xusb` is fine and these two are not:
//!   it is NOT a HID minidriver, so nothing sits above it.
//! * ✅ **The serial-number string.** `HidD_GetSerialNumberString` succeeds on a zero-access handle
//!   and returns driver-authored text. Verified end to end: `pf_mouse` answered `PFCP:3:0:7296`, and
//!   pid 7296 was a genuine service-spawned `WUDFHost.exe`.
//!
//! So `pf_mouse` serves its proof AS its serial (its `PFMOUSE00` was inert). The pads deliberately do
//! NOT: their serials are what SDL and Steam dedup controllers on, and Steam is already known to
//! mangle a pad's displayed name over serial FORMAT alone. `pf_gamepad` therefore has no working
//! device-stack transport on this Windows build — see `ProofTransport::MailboxUnproven` on the host
//! side for the residual that leaves, and option B (a vendor feature report, which costs a
//! report-descriptor change) if it ever needs closing.
