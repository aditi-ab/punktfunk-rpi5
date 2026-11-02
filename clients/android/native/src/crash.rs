//! What a dead run's exit record says, as log lines. Android hands the next launch a trace
//! through `ApplicationExitInfo.getTraceInputStream()`: a `tombstone.proto` message for a native
//! crash (API 31+), the ART stack dump as text for an ANR. The field numbers below are those of
//! `system/core/debuggerd/proto/tombstone.proto`.

use std::fmt::Write;

/// `ApplicationExitInfo.REASON_CRASH_NATIVE`.
const REASON_CRASH_NATIVE: i32 = 5;
/// `ApplicationExitInfo.REASON_ANR`.
const REASON_ANR: i32 = 6;

/// The lines worth keeping from an exit `trace`: the crashing thread of a native crash, the main
/// thread of an ANR. Empty for any other `reason`.
pub(crate) fn trace_lines(reason: i32, trace: &[u8]) -> Vec<String> {
    match reason {
        REASON_CRASH_NATIVE => tombstone_lines(trace),
        REASON_ANR => String::from_utf8_lossy(trace)
            .lines()
            .skip_while(|line| !line.starts_with("\"main\""))
            .take_while(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Signal, abort message, causes and the crashing thread's backtrace, in debuggerd's text shape
/// behind its banner and ABI line, so `ndk-stack -i` symbolizes a bundle as it is.
fn tombstone_lines(tombstone: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut arch = 0;
    let mut tid = 0;
    let mut threads = Vec::new();
    for (field, value) in fields(tombstone) {
        match (field, value) {
            (1, Value::Int(v)) => arch = v,
            (6, Value::Int(v)) => tid = v,
            (10, Value::Bytes(signal)) => lines.push(signal_line(signal)),
            (14, Value::Bytes(message)) => {
                lines.push(format!(
                    "Abort message: '{}'",
                    String::from_utf8_lossy(message)
                ));
            }
            (15, Value::Bytes(cause)) => lines.push(format!("Cause: {}", string(cause, 1))),
            // `map<uint32, Thread>`: each entry carries its thread in field 2.
            (16, Value::Bytes(entry)) => threads.extend(bytes(entry, 2)),
            _ => {}
        }
    }
    if let Some(thread) = threads.into_iter().find(|thread| int(thread, 1) == tid) {
        lines.push(format!("backtrace of tid {tid} ({}):", string(thread, 2)));
        lines.extend(
            repeated(thread, 4)
                .enumerate()
                .map(|(i, frame)| frame_line(i, frame)),
        );
    }
    if lines.is_empty() {
        return lines;
    }
    // `Architecture`; proto3 leaves out its zero, ARM32.
    let abi = match arch {
        0 => "arm",
        1 => "arm64",
        2 => "x86",
        3 => "x86_64",
        4 => "riscv64",
        _ => "?",
    };
    // `ndk-stack` starts at debuggerd's banner and reads the ABI only after a `: `.
    let banner = "*** *** *** *** *** *** *** *** *** *** *** *** *** *** *** ***";
    [banner.to_owned(), format!("tombstone: ABI: '{abi}'")]
        .into_iter()
        .chain(lines)
        .collect()
}

/// `signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0`. A negative `int32` arrives as a
/// ten-byte varint whose low half is the value, hence the `as i32`.
fn signal_line(signal: &[u8]) -> String {
    let fault = if int(signal, 8) == 0 {
        "--------".to_owned()
    } else {
        format!("0x{:x}", int(signal, 9))
    };
    format!(
        "signal {} ({}), code {} ({}), fault addr {fault}",
        int(signal, 1) as i32,
        string(signal, 2),
        int(signal, 3) as i32,
        string(signal, 4)
    )
}

/// `  #00 pc 000000000001a2b4  /data/app/…/libpunktfunk_android.so (fn+12) (BuildId: …)`.
fn frame_line(index: usize, frame: &[u8]) -> String {
    let mut line = format!(
        "  #{index:02} pc {:016x}  {}",
        int(frame, 1),
        string(frame, 6)
    );
    let function = string(frame, 4);
    if !function.is_empty() {
        let _ = write!(line, " ({function}+{})", int(frame, 5));
    }
    let build_id = string(frame, 8);
    if !build_id.is_empty() {
        let _ = write!(line, " (BuildId: {build_id})");
    }
    line
}

enum Value<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

/// One message's fields in wire order. Stops at the first byte that does not parse, so a cut
/// tombstone still yields what came before the cut.
fn fields(mut buf: &[u8]) -> impl Iterator<Item = (u64, Value<'_>)> {
    std::iter::from_fn(move || {
        let key = varint(&mut buf)?;
        let value = match key & 7 {
            0 => Value::Int(varint(&mut buf)?),
            1 => {
                let (v, rest) = buf.split_first_chunk::<8>()?;
                buf = rest;
                Value::Int(u64::from_le_bytes(*v))
            }
            2 => {
                let len = usize::try_from(varint(&mut buf)?).ok()?;
                let (v, rest) = buf.split_at_checked(len)?;
                buf = rest;
                Value::Bytes(v)
            }
            5 => {
                let (v, rest) = buf.split_first_chunk::<4>()?;
                buf = rest;
                Value::Int(u32::from_le_bytes(*v).into())
            }
            _ => return None,
        };
        Some((key >> 3, value))
    })
}

fn varint(buf: &mut &[u8]) -> Option<u64> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = buf.split_first()?;
        *buf = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn int(message: &[u8], field: u64) -> u64 {
    fields(message)
        .find_map(|(n, value)| match value {
            Value::Int(v) if n == field => Some(v),
            _ => None,
        })
        .unwrap_or(0)
}

fn repeated(message: &[u8], field: u64) -> impl Iterator<Item = &[u8]> {
    fields(message).filter_map(move |(n, value)| match value {
        Value::Bytes(v) if n == field => Some(v),
        _ => None,
    })
}

fn bytes(message: &[u8], field: u64) -> Option<&[u8]> {
    repeated(message, field).next()
}

fn string(message: &[u8], field: u64) -> String {
    String::from_utf8_lossy(bytes(message, field).unwrap_or_default()).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        while v >= 0x80 {
            out.push(v as u8 | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
        out
    }

    fn int_field(field: u64, v: u64) -> Vec<u8> {
        [enc(field << 3), enc(v)].concat()
    }

    fn msg_field(field: u64, body: &[u8]) -> Vec<u8> {
        [enc(field << 3 | 2), enc(body.len() as u64), body.to_vec()].concat()
    }

    fn tombstone() -> Vec<u8> {
        let lib = b"/data/app/io.unom.punktfunk/lib/arm64/libpunktfunk_android.so";
        let ours = [
            int_field(1, 0x1a2b4),
            msg_field(4, b"decode"),
            int_field(5, 12),
            msg_field(6, lib),
            msg_field(8, b"9f3c"),
        ]
        .concat();
        let libc = [int_field(1, 0x5f0), msg_field(6, b"/apex/libc.so")].concat();
        let crashed = [
            int_field(1, 42),
            msg_field(2, b"pf-decode"),
            msg_field(4, &ours),
            msg_field(4, &libc),
        ];
        let other = [int_field(1, 7), msg_field(2, b"main")].concat();
        let signal = [
            int_field(1, 6),
            msg_field(2, b"SIGABRT"),
            int_field(3, -6i64 as u64),
            msg_field(4, b"SI_TKILL"),
        ];
        [
            int_field(1, 1), // ARM64
            int_field(6, 42),
            msg_field(10, &signal.concat()),
            msg_field(14, b"boom"),
            msg_field(16, &[int_field(1, 7), msg_field(2, &other)].concat()),
            msg_field(
                16,
                &[int_field(1, 42), msg_field(2, &crashed.concat())].concat(),
            ),
        ]
        .concat()
    }

    #[test]
    fn a_native_crash_reads_as_debuggerd_prints_it() {
        assert_eq!(
            trace_lines(REASON_CRASH_NATIVE, &tombstone()),
            [
                "*** *** *** *** *** *** *** *** *** *** *** *** *** *** *** ***",
                "tombstone: ABI: 'arm64'",
                "signal 6 (SIGABRT), code -6 (SI_TKILL), fault addr --------",
                "Abort message: 'boom'",
                "backtrace of tid 42 (pf-decode):",
                "  #00 pc 000000000001a2b4  \
                 /data/app/io.unom.punktfunk/lib/arm64/libpunktfunk_android.so (decode+12) \
                 (BuildId: 9f3c)",
                "  #01 pc 00000000000005f0  /apex/libc.so",
            ]
        );
    }

    #[test]
    fn a_cut_tombstone_keeps_what_came_before_the_cut() {
        let tombstone = tombstone();
        let lines = trace_lines(REASON_CRASH_NATIVE, &tombstone[..tombstone.len() - 3]);
        assert_eq!(lines.len(), 4, "{lines:?}"); // banner, ABI, the signal, the abort message
        assert!(trace_lines(REASON_CRASH_NATIVE, &[0xff; 3]).is_empty());
    }

    #[test]
    fn an_anr_keeps_the_main_thread() {
        let trace = "----- pid 1 at 2026-10-19 -----\n\
                     \"main\" prio=5 tid=1 Blocked\n  at a.B.c(B.kt:1)\n\n\
                     \"Signal Catcher\" daemon prio=10 tid=2 Runnable\n";
        assert_eq!(
            trace_lines(REASON_ANR, trace.as_bytes()),
            ["\"main\" prio=5 tid=1 Blocked", "  at a.B.c(B.kt:1)"]
        );
        assert!(trace_lines(4, trace.as_bytes()).is_empty()); // REASON_CRASH: no trace to read
    }
}
