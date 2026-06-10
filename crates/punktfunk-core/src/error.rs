//! Error type and the stable C ABI status codes it maps to.

use thiserror::Error;

/// The core's internal error type. Crosses the C ABI as a [`PunktfunkStatus`] code.
#[derive(Debug, Error)]
pub enum PunktfunkError {
    #[error("invalid argument: {0}")]
    InvalidArg(&'static str),
    #[error("fec error: {0}")]
    Fec(#[from] crate::fec::FecError),
    #[error("crypto seal/open failed")]
    Crypto,
    #[error("malformed packet")]
    BadPacket,
    #[error("no complete frame available yet")]
    NoFrame,
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out")]
    Timeout,
    #[error("session closed")]
    Closed,
}

pub type Result<T> = core::result::Result<T, PunktfunkError>;

/// Stable C ABI status codes. `Ok` is 0; all errors are negative so callers can
/// test `rc < 0`. Do not renumber existing variants — only append.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunktfunkStatus {
    Ok = 0,
    InvalidArg = -1,
    Fec = -2,
    Crypto = -3,
    BadPacket = -4,
    NoFrame = -5,
    Unsupported = -6,
    Io = -7,
    NullPointer = -8,
    Timeout = -9,
    Closed = -10,
    Panic = -99,
}

impl PunktfunkError {
    /// Map to the C ABI status code.
    pub fn status(&self) -> PunktfunkStatus {
        match self {
            PunktfunkError::InvalidArg(_) => PunktfunkStatus::InvalidArg,
            PunktfunkError::Fec(_) => PunktfunkStatus::Fec,
            PunktfunkError::Crypto => PunktfunkStatus::Crypto,
            PunktfunkError::BadPacket => PunktfunkStatus::BadPacket,
            PunktfunkError::NoFrame => PunktfunkStatus::NoFrame,
            PunktfunkError::Unsupported(_) => PunktfunkStatus::Unsupported,
            PunktfunkError::Io(_) => PunktfunkStatus::Io,
            PunktfunkError::Timeout => PunktfunkStatus::Timeout,
            PunktfunkError::Closed => PunktfunkStatus::Closed,
        }
    }
}
