//! Raw passthrough for a Steam Controller 2 held by an SDL slot. SDL's Triton driver still
//! feeds the typed plane (escape chord, ring, menus); a second hidapi handle on the same node
//! forwards every input report verbatim to the host's as-is `28DE:1302` pad and replays Steam's
//! writes on the physical controller. Trackpads, gyro and haptics exist only on this path: the
//! host's typed fallback carries buttons, sticks and triggers.
//!
//! Same contract as Android's `Sc2Capture` and Apple's: the IMU gate, wireless reports kept
//! off the wire, writes forwarded unchanged.

use punktfunk_core::client::NativeClient;
use punktfunk_core::config::GamepadPref;
use punktfunk_core::quic::{RichInput, HID_RAW_FEATURE, HID_RAW_OUTPUT, HID_REPORT_MAX};
use sdl3::sys::hidapi as hid;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;

/// SDL `ETritonReportIDTypes`.
const ID_STATE: u8 = 0x42;
const ID_STATE_BLE: u8 = 0x45;
const ID_WIRELESS_X: u8 = 0x46;
const ID_STATE_TIMESTAMP: u8 = 0x47;
const ID_WIRELESS: u8 = 0x79;

/// SDL `TritonButtons` bits that [`Gate::system_forward`] keeps local.
const BTN_QAM: u32 = 0x0000_0010;
const BTN_STEAM: u32 = 0x0001_0000;

/// A queued host write waits at most this long behind a read.
const READ_TIMEOUT_MS: i32 = 4;

/// `SETTING_ENABLE_RAW_JOYSTICK` off: a pad left in raw mode reports ADC stick values that
/// read as a few percent of travel. Steam sends the same at init; SDL does not.
const NORMALIZE_JOYSTICKS: [u8; 64] = {
    let mut r = [0u8; 64];
    r[0] = 0x01; // feature report id
    r[1] = 0x87; // ID_SET_SETTINGS_VALUES
    r[2] = 3; // one {u8 num, u16 value}
    r[3] = 0x2E; // SETTING_ENABLE_RAW_JOYSTICK, value 0
    r
};

/// Wired `1302` and BLE `1303` pads are the controller itself; `1304`/`1305` are Puck dongles.
pub(crate) fn pref_for(vid: u16, pid: u16) -> Option<GamepadPref> {
    match (vid, pid) {
        (0x28DE, 0x1302 | 0x1303) => Some(GamepadPref::SteamController2),
        (0x28DE, 0x1304 | 0x1305) => Some(GamepadPref::SteamController2Puck),
        _ => None,
    }
}

pub(crate) fn is_sc2(pref: GamepadPref) -> bool {
    matches!(
        pref,
        GamepadPref::SteamController2 | GamepadPref::SteamController2Puck
    )
}

#[derive(Clone, Copy)]
pub(crate) struct Gate {
    /// Overlay owns the pad: state reports go out neutral so nothing stays held on the host.
    pub masked: bool,
    /// Off: Steam and QAM stay with the local shell, as on the typed plane.
    pub system_forward: bool,
}

enum Cmd {
    Write(u8, Vec<u8>),
    Gate(Gate),
}

/// The reader thread owns the handle; dropping this hangs up and joins it.
pub(crate) struct Sc2Capture {
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

impl Sc2Capture {
    /// `path` is the SDL slot's HID path. `None` when the node will not open (not a HIDAPI
    /// device, no permission): the slot keeps the typed plane.
    pub(crate) fn open(
        path: &str,
        client: Arc<NativeClient>,
        pad: u8,
        gate: Gate,
    ) -> Option<Sc2Capture> {
        let c_path = std::ffi::CString::new(path).ok()?;
        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call.
        let dev = Dev(unsafe { hid::SDL_hid_open_path(c_path.as_ptr()) });
        if dev.0.is_null() {
            tracing::warn!(path, error = %sdl3::get_error(), "open steam controller 2 hid node");
            return None;
        }
        dev.send_feature(&NORMALIZE_JOYSTICKS);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pf-sc2-raw".into())
            .spawn(move || run(dev, &rx, &client, pad, gate))
            .map_err(|e| tracing::warn!(error = %e, "spawn steam controller 2 reader"))
            .ok()?;
        Some(Sc2Capture {
            tx,
            thread: Some(thread),
        })
    }

    /// Host `HidOutput::HidRaw`: `data` is the full report, id first.
    pub(crate) fn write(&self, kind: u8, data: Vec<u8>) {
        let _ = self.tx.send(Cmd::Write(kind, data));
    }

    pub(crate) fn set_gate(&self, gate: Gate) {
        let _ = self.tx.send(Cmd::Gate(gate));
    }
}

impl Drop for Sc2Capture {
    fn drop(&mut self) {
        // Replacing the sender disconnects the reader's channel; it exits and closes the node.
        self.tx = std::sync::mpsc::channel().0;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Dev(*mut hid::SDL_hid_device);

// SAFETY: the handle moves into the reader thread once and is used and closed only there.
unsafe impl Send for Dev {}

impl Dev {
    fn send_feature(&self, r: &[u8]) -> i32 {
        // SAFETY: `self.0` is an open handle only this thread uses; `r` is valid for its length.
        unsafe { hid::SDL_hid_send_feature_report(self.0, r.as_ptr(), r.len()) }
    }

    fn write(&self, kind: u8, r: &[u8]) -> i32 {
        match kind {
            // SAFETY: as in `send_feature`.
            HID_RAW_OUTPUT => unsafe { hid::SDL_hid_write(self.0, r.as_ptr(), r.len()) },
            HID_RAW_FEATURE => self.send_feature(r),
            _ => 0,
        }
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: an open handle, closed once, on the thread that owns it.
            unsafe { hid::SDL_hid_close(self.0) };
        }
    }
}

fn run(dev: Dev, rx: &Receiver<Cmd>, client: &NativeClient, pad: u8, mut gate: Gate) {
    let mut imu = ImuGate::default();
    let mut buf = [0u8; HID_REPORT_MAX];
    loop {
        loop {
            match rx.try_recv() {
                Ok(Cmd::Write(kind, data)) => {
                    if dev.write(kind, &data) < 0 {
                        let id = data.first().copied().unwrap_or(0);
                        tracing::debug!(pad, kind, id, "steam controller 2 write refused");
                    }
                }
                Ok(Cmd::Gate(g)) => gate = g,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        // SAFETY: open handle owned by this thread; `buf` is writable for its full length.
        let n = unsafe {
            hid::SDL_hid_read_timeout(dev.0, buf.as_mut_ptr(), buf.len(), READ_TIMEOUT_MS)
        };
        if n < 0 {
            // Unplug: SDL removes the slot too, which drops the capture.
            tracing::info!(pad, "steam controller 2 hid node closed");
            return;
        }
        let n = n as usize;
        if n == 0 || !filter_report(&mut buf[..n], gate, &mut imu) {
            continue;
        }
        let _ = client.send_rich_input(RichInput::HidReport {
            pad,
            len: n as u8,
            data: buf,
        });
    }
}

/// Gate one report in place. False for a report that stays off the wire: slot lifecycle rides
/// SDL hotplug, and the host queues its own Puck connect edge.
fn filter_report(r: &mut [u8], gate: Gate, imu: &mut ImuGate) -> bool {
    match r[0] {
        ID_WIRELESS | ID_WIRELESS_X => return false,
        ID_STATE | ID_STATE_BLE | ID_STATE_TIMESTAMP if r.len() >= 6 => {
            if gate.masked {
                r[2..].fill(0);
            } else if !gate.system_forward {
                let b = u32::from_le_bytes([r[2], r[3], r[4], r[5]]) & !(BTN_STEAM | BTN_QAM);
                r[2..6].copy_from_slice(&b.to_le_bytes());
            }
        }
        _ => {}
    }
    imu.apply(r);
    true
}

/// The pad streams IMU only after Steam writes `SETTING_IMU_MODE`. Until then the block and its
/// timestamp are a frozen resting sample, which Steam's desktop gyro-mouse reads as constant
/// rotation. Pass it only while the timestamp moves; `0x47` diverges from byte 18 and passes.
#[derive(Default)]
struct ImuGate {
    last: u32,
    seen: bool,
    stale: u8,
}

impl ImuGate {
    /// `TritonMTUNoQuat_t.imu` (struct offset 29 + id byte): u32 timestamp, 3× accel, 3× gyro.
    const OFFSET: usize = 30;
    const LEN: usize = 16;
    /// Three repeats still pass (report rate beats the IMU rate); the fourth freezes.
    const STALE_LIMIT: u8 = 4;

    fn apply(&mut self, r: &mut [u8]) {
        if r.len() < Self::OFFSET + Self::LEN || !matches!(r[0], ID_STATE | ID_STATE_BLE) {
            return;
        }
        let o = Self::OFFSET;
        let ts = u32::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]]);
        let live = if !std::mem::replace(&mut self.seen, true) {
            self.stale = Self::STALE_LIMIT;
            false
        } else if ts != self.last {
            self.stale = 0;
            true
        } else {
            self.stale = (self.stale + 1).min(Self::STALE_LIMIT);
            self.stale < Self::STALE_LIMIT
        };
        self.last = ts;
        if !live {
            r[o..o + Self::LEN].fill(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: Gate = Gate {
        masked: false,
        system_forward: true,
    };

    fn state(ts: u32, buttons: u32) -> [u8; 54] {
        let mut r = [0u8; 54];
        r[0] = ID_STATE;
        r[1] = 7;
        r[2..6].copy_from_slice(&buttons.to_le_bytes());
        r[10] = 0x40; // left stick x
        r[30..34].copy_from_slice(&ts.to_le_bytes());
        r[36] = 0x11; // gyro
        r
    }

    #[test]
    fn detects_every_sc2_identity() {
        assert_eq!(
            pref_for(0x28DE, 0x1302),
            Some(GamepadPref::SteamController2)
        );
        assert_eq!(
            pref_for(0x28DE, 0x1303),
            Some(GamepadPref::SteamController2)
        );
        assert_eq!(
            pref_for(0x28DE, 0x1304),
            Some(GamepadPref::SteamController2Puck)
        );
        assert_eq!(
            pref_for(0x28DE, 0x1305),
            Some(GamepadPref::SteamController2Puck)
        );
        assert_eq!(pref_for(0x28DE, 0x1205), None);
    }

    #[test]
    fn frozen_imu_is_zeroed_and_a_moving_one_passes() {
        let mut imu = ImuGate::default();
        let mut r = state(100, 0);
        assert!(filter_report(&mut r, OPEN, &mut imu));
        assert_eq!(r[36], 0, "first sample is unproven");
        let mut r = state(100, 0);
        filter_report(&mut r, OPEN, &mut imu);
        assert_eq!(r[36], 0, "frozen timestamp");
        let mut r = state(101, 0);
        filter_report(&mut r, OPEN, &mut imu);
        assert_eq!(r[36], 0x11, "moving timestamp");
        for _ in 0..3 {
            let mut r = state(101, 0);
            filter_report(&mut r, OPEN, &mut imu);
            assert_eq!(r[36], 0x11, "short repeats pass");
        }
        let mut r = state(101, 0);
        filter_report(&mut r, OPEN, &mut imu);
        assert_eq!(r[36], 0, "fourth repeat freezes");
    }

    #[test]
    fn mask_neutralises_state_but_keeps_id_and_seq() {
        let mut r = state(5, 0x1);
        let masked = Gate {
            masked: true,
            system_forward: true,
        };
        assert!(filter_report(&mut r, masked, &mut ImuGate::default()));
        assert_eq!((r[0], r[1]), (ID_STATE, 7));
        assert!(r[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn local_system_buttons_stay_off_the_wire() {
        let mut r = state(5, BTN_STEAM | BTN_QAM | 0x1);
        let local = Gate {
            masked: false,
            system_forward: false,
        };
        filter_report(&mut r, local, &mut ImuGate::default());
        assert_eq!(u32::from_le_bytes([r[2], r[3], r[4], r[5]]), 0x1);
        assert_eq!(r[10], 0x40);
    }

    #[test]
    fn wireless_status_never_reaches_the_host() {
        let mut imu = ImuGate::default();
        assert!(!filter_report(&mut [ID_WIRELESS, 0x01], OPEN, &mut imu));
        assert!(!filter_report(&mut [ID_WIRELESS_X, 0x02], OPEN, &mut imu));
        assert!(filter_report(&mut [0x43, 80], OPEN, &mut imu));
    }
}
