//! Decoder-input capture behind `PUNKTFUNK_DUMP_VIDEO` (fixture corpus for the
//! native-decode program, design/client-native-decode.md M0).
//!
//! Writes every AU exactly as the pump hands it to [`crate::video::Decoder::decode_frame`]:
//! the data file is the raw concatenation (a valid Annex-B / OBU stream for clean
//! captures), and the sidecar `.idx` keeps what a byte stream cannot carry — the exact
//! AU boundaries plus the wire `flags`/`complete` bits — one `offset len flags complete`
//! line per AU, so parser fixtures never have to re-derive framing from start codes.
//!
//! Capture is best-effort by design: any I/O error logs once and disables the dump for
//! the rest of the session; the streaming path is never failed on its account. The
//! final buffered tail flushes on drop (session end), errors swallowed — a truncated
//! last AU in a debug capture is acceptable, a stream torn down over one is not.

use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

pub(crate) struct AuDump {
    data: BufWriter<std::fs::File>,
    idx: BufWriter<std::fs::File>,
    offset: u64,
}

/// Wire-codec byte → fixture file extension (also the corpus naming convention).
fn codec_ext(codec: u8) -> &'static str {
    match codec {
        punktfunk_core::quic::CODEC_H264 => "h264",
        punktfunk_core::quic::CODEC_HEVC => "h265",
        punktfunk_core::quic::CODEC_AV1 => "av1",
        punktfunk_core::quic::CODEC_PYROWAVE => "pyrowave",
        _ => "bin",
    }
}

impl AuDump {
    /// Read `PUNKTFUNK_DUMP_VIDEO`; `None` (the overwhelmingly common case) means the
    /// variable is unset or the capture files could not be created — both already logged
    /// where they matter.
    pub(crate) fn from_env(codec: u8) -> Option<AuDump> {
        let dir = std::env::var_os("PUNKTFUNK_DUMP_VIDEO")?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self::create(Path::new(&dir), &format!("au-{stamp}"), codec)
    }

    fn create(dir: &Path, base: &str, codec: u8) -> Option<AuDump> {
        let ext = codec_ext(codec);
        let data_path = dir.join(format!("{base}.{ext}"));
        let idx_path = dir.join(format!("{base}.{ext}.idx"));
        let made = std::fs::create_dir_all(dir).and_then(|()| {
            Ok((
                std::fs::File::create(&data_path)?,
                std::fs::File::create(&idx_path)?,
            ))
        });
        match made {
            Ok((data, idx)) => {
                tracing::info!(
                    path = %data_path.display(),
                    "PUNKTFUNK_DUMP_VIDEO: capturing decoder input"
                );
                Some(AuDump {
                    data: BufWriter::new(data),
                    idx: BufWriter::new(idx),
                    offset: 0,
                })
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    dir = %dir.display(),
                    "PUNKTFUNK_DUMP_VIDEO set but capture files could not be created"
                );
                None
            }
        }
    }

    /// Append one AU. Returns `false` once the dump should be dropped (error logged).
    pub(crate) fn write(&mut self, au: &[u8], flags: u32, complete: bool) -> bool {
        let r = self.data.write_all(au).and_then(|()| {
            writeln!(
                self.idx,
                "{} {} {:#x} {}",
                self.offset,
                au.len(),
                flags,
                u8::from(complete)
            )
        });
        self.offset += au.len() as u64;
        match r {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "PUNKTFUNK_DUMP_VIDEO write failed — capture disabled");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capture_is_a_byte_faithful_concatenation_with_a_boundary_index() {
        let dir = std::env::temp_dir().join(format!("pf-au-dump-test-{}", std::process::id()));
        let mut dump = AuDump::create(&dir, "t", punktfunk_core::quic::CODEC_HEVC)
            .expect("capture files should be creatable in a temp dir");
        assert!(dump.write(&[1, 2, 3], 0x04, true));
        assert!(dump.write(&[9, 8], 0x00, false));
        drop(dump);

        let data = std::fs::read(dir.join("t.h265")).unwrap();
        let idx = std::fs::read_to_string(dir.join("t.h265.idx")).unwrap();
        assert_eq!(data, &[1, 2, 3, 9, 8]);
        assert_eq!(idx, "0 3 0x4 1\n3 2 0x0 0\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
