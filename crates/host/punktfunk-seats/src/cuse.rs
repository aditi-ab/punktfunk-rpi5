//! The FUSE/CUSE kernel protocol (`include/uapi/linux/fuse.h`) as the uinput proxy speaks it
//! over `/dev/cuse`: one request per read, one reply per write, in the machine's byte order.
//! Only the operations a character device sees are parsed; the proxy answers the rest
//! `ENOSYS`.

/// `struct fuse_in_header`.
pub const IN_HEADER_LEN: usize = 40;
/// `struct fuse_out_header`.
pub const OUT_HEADER_LEN: usize = 16;

pub(crate) const FUSE_OPEN: u32 = 14;
pub(crate) const FUSE_READ: u32 = 15;
pub(crate) const FUSE_WRITE: u32 = 16;
pub(crate) const FUSE_RELEASE: u32 = 18;
pub(crate) const FUSE_FLUSH: u32 = 25;
pub(crate) const FUSE_INTERRUPT: u32 = 36;
pub(crate) const FUSE_IOCTL: u32 = 39;
pub(crate) const FUSE_POLL: u32 = 40;
pub(crate) const CUSE_INIT: u32 = 4096;
const FUSE_NOTIFY_POLL: i32 = 1;

const FUSE_KERNEL_VERSION: u32 = 7;
/// 7.16 brought `struct fuse_ioctl_iovec`, the retry layout [`ioctl_retry`] writes.
pub const MIN_MINOR: u32 = 16;
const CUSE_UNRESTRICTED_IOCTL: u32 = 1 << 0;
const FUSE_IOCTL_RETRY: u32 = 1 << 2;
/// The caller is a 32-bit process.
pub const FUSE_IOCTL_32BIT: u32 = 1 << 3;
/// A poller waits: answer later with [`notify_poll`].
pub const FUSE_POLL_SCHEDULE_NOTIFY: u32 = 1 << 0;

/// One request off `/dev/cuse`.
#[derive(Debug, PartialEq, Eq)]
pub enum Request<'a> {
    Init {
        major: u32,
        minor: u32,
    },
    /// `flags` are the caller's `open(2)` flags.
    Open {
        flags: u32,
    },
    Release {
        fh: u64,
    },
    Flush,
    Read {
        fh: u64,
        size: u32,
        flags: u32,
    },
    Write {
        fh: u64,
        data: &'a [u8],
    },
    /// `data` is what an earlier retry asked the kernel to copy in.
    Ioctl {
        fh: u64,
        flags: u32,
        cmd: u32,
        arg: u64,
        in_size: u32,
        out_size: u32,
        data: &'a [u8],
    },
    Poll {
        fh: u64,
        kh: u64,
        flags: u32,
    },
    /// The kernel gave up on request `unique`; this one wants no answer.
    Interrupt {
        unique: u64,
    },
    Other(u32),
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(*b.get(at..)?.first_chunk()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(*b.get(at..)?.first_chunk()?))
}

/// `(unique, request)` of one whole read; `None` when it is short or its length disagrees.
pub fn parse(buf: &[u8]) -> Option<(u64, Request<'_>)> {
    if u32_at(buf, 0)? as usize != buf.len() {
        return None;
    }
    let opcode = u32_at(buf, 4)?;
    let unique = u64_at(buf, 8)?;
    let body = buf.get(IN_HEADER_LEN..)?;
    let request = match opcode {
        CUSE_INIT => Request::Init {
            major: u32_at(body, 0)?,
            minor: u32_at(body, 4)?,
        },
        FUSE_OPEN => Request::Open {
            flags: u32_at(body, 0)?,
        },
        FUSE_RELEASE => Request::Release {
            fh: u64_at(body, 0)?,
        },
        FUSE_FLUSH => Request::Flush,
        FUSE_READ => Request::Read {
            fh: u64_at(body, 0)?,
            size: u32_at(body, 16)?,
            flags: u32_at(body, 32)?,
        },
        FUSE_WRITE => {
            let size = u32_at(body, 16)? as usize;
            let data = body.get(40..)?;
            if data.len() != size {
                return None;
            }
            Request::Write {
                fh: u64_at(body, 0)?,
                data,
            }
        }
        FUSE_IOCTL => {
            let in_size = u32_at(body, 24)?;
            let data = body.get(32..)?;
            if data.len() != in_size as usize {
                return None;
            }
            Request::Ioctl {
                fh: u64_at(body, 0)?,
                flags: u32_at(body, 8)?,
                cmd: u32_at(body, 12)?,
                arg: u64_at(body, 16)?,
                in_size,
                out_size: u32_at(body, 28)?,
                data,
            }
        }
        FUSE_POLL => Request::Poll {
            fh: u64_at(body, 0)?,
            kh: u64_at(body, 8)?,
            flags: u32_at(body, 16)?,
        },
        FUSE_INTERRUPT => Request::Interrupt {
            unique: u64_at(body, 0)?,
        },
        other => Request::Other(other),
    };
    Some((unique, request))
}

/// `fuse_out_header` then `parts`.
fn message(unique: u64, error: i32, parts: &[&[u8]]) -> Vec<u8> {
    let len = OUT_HEADER_LEN + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&(len as u32).to_ne_bytes());
    out.extend_from_slice(&error.to_ne_bytes());
    out.extend_from_slice(&unique.to_ne_bytes());
    parts.iter().for_each(|p| out.extend_from_slice(p));
    out
}

/// A success carrying `parts` as the operation's reply.
pub fn reply(unique: u64, parts: &[&[u8]]) -> Vec<u8> {
    message(unique, 0, parts)
}

/// `errno` as the operation's result: the header alone.
pub fn error(unique: u64, errno: i32) -> Vec<u8> {
    message(unique, -errno, &[])
}

/// `struct cuse_init_out`, unrestricted ioctls, then `DEVNAME=<devname>` NUL-terminated. A zero
/// `dev_major` lets the kernel pick the device number.
pub fn init_reply(unique: u64, minor: u32, max_io: u32, devname: &str) -> Vec<u8> {
    let mut init = [0u8; 72];
    let fields = [
        FUSE_KERNEL_VERSION,
        minor,
        0,
        CUSE_UNRESTRICTED_IOCTL,
        max_io,
        max_io,
    ];
    for (i, field) in fields.iter().enumerate() {
        init[i * 4..i * 4 + 4].copy_from_slice(&field.to_ne_bytes());
    }
    let info = format!("DEVNAME={devname}\0");
    reply(unique, &[&init, info.as_bytes()])
}

/// `struct fuse_open_out`.
pub fn open_reply(unique: u64, fh: u64) -> Vec<u8> {
    reply(unique, &[&fh.to_ne_bytes(), &[0; 8]])
}

/// `struct fuse_write_out`.
pub fn write_reply(unique: u64, size: u32) -> Vec<u8> {
    reply(unique, &[&size.to_ne_bytes(), &[0; 4]])
}

/// `struct fuse_poll_out`.
pub fn poll_reply(unique: u64, revents: u32) -> Vec<u8> {
    reply(unique, &[&revents.to_ne_bytes(), &[0; 4]])
}

/// `struct fuse_ioctl_out` with `result`, then what the kernel copies out to the caller.
pub fn ioctl_reply(unique: u64, result: i32, out: &[u8]) -> Vec<u8> {
    let mut head = [0u8; 16];
    head[..4].copy_from_slice(&result.to_ne_bytes());
    reply(unique, &[&head, out])
}

/// Asks the kernel to call again with `ins` copied in and room for `outs`, each `(address,
/// length)` in the caller's memory.
pub fn ioctl_retry(unique: u64, ins: &[(u64, u64)], outs: &[(u64, u64)]) -> Vec<u8> {
    let mut body = Vec::with_capacity(16 + 16 * (ins.len() + outs.len()));
    body.extend_from_slice(&0i32.to_ne_bytes());
    body.extend_from_slice(&FUSE_IOCTL_RETRY.to_ne_bytes());
    body.extend_from_slice(&(ins.len() as u32).to_ne_bytes());
    body.extend_from_slice(&(outs.len() as u32).to_ne_bytes());
    for (base, len) in ins.iter().chain(outs) {
        body.extend_from_slice(&base.to_ne_bytes());
        body.extend_from_slice(&len.to_ne_bytes());
    }
    reply(unique, &[&body])
}

/// `FUSE_NOTIFY_POLL`: wakes whoever polls the file the kernel calls `kh`.
pub fn notify_poll(kh: u64) -> Vec<u8> {
    message(0, FUSE_NOTIFY_POLL, &[&kh.to_ne_bytes()])
}

/// A request as the kernel lays it out, for tests: header, then `body`.
#[cfg(test)]
pub fn request(opcode: u32, unique: u64, body: &[u8]) -> Vec<u8> {
    let len = IN_HEADER_LEN + body.len();
    let mut out = vec![0u8; IN_HEADER_LEN];
    out[..4].copy_from_slice(&(len as u32).to_ne_bytes());
    out[4..8].copy_from_slice(&opcode.to_ne_bytes());
    out[8..16].copy_from_slice(&unique.to_ne_bytes());
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le32(v: u32) -> [u8; 4] {
        v.to_ne_bytes()
    }

    fn le64(v: u64) -> [u8; 8] {
        v.to_ne_bytes()
    }

    #[test]
    fn requests_parse_at_the_header_struct_offsets() {
        let init = request(
            CUSE_INIT,
            1,
            &[le32(7), le32(45), le32(0), le32(1)].concat(),
        );
        assert_eq!(
            parse(&init),
            Some((
                1,
                Request::Init {
                    major: 7,
                    minor: 45
                }
            ))
        );
        // fuse_read_in: fh, offset, size, read_flags, lock_owner, flags, padding — 40 bytes.
        let read = [
            &le64(9)[..],
            &le64(0),
            &le32(32),
            &le32(0),
            &le64(0),
            &le32(0o4002),
            &le32(0),
        ]
        .concat();
        assert_eq!(read.len(), 40);
        assert_eq!(
            parse(&request(FUSE_READ, 2, &read)),
            Some((
                2,
                Request::Read {
                    fh: 9,
                    size: 32,
                    flags: 0o4002
                }
            ))
        );
        // fuse_write_in is 40 bytes too, its data after it.
        let mut write = [&le64(9)[..], &le64(0), &le32(3), &[0; 20]].concat();
        write.extend_from_slice(b"abc");
        assert_eq!(
            parse(&request(FUSE_WRITE, 3, &write)),
            Some((
                3,
                Request::Write {
                    fh: 9,
                    data: b"abc"
                }
            ))
        );
        write.push(0);
        assert_eq!(
            parse(&request(FUSE_WRITE, 3, &write)),
            None,
            "size disagrees"
        );
        // fuse_ioctl_in: fh, flags, cmd, arg, in_size, out_size — 32 bytes.
        let mut ioctl = [
            &le64(9)[..],
            &le32(FUSE_IOCTL_32BIT),
            &le32(0x4004_5565),
            &le64(0x130),
            &le32(2),
            &le32(0),
        ]
        .concat();
        ioctl.extend_from_slice(&[7, 8]);
        assert_eq!(
            parse(&request(FUSE_IOCTL, 4, &ioctl)),
            Some((
                4,
                Request::Ioctl {
                    fh: 9,
                    flags: FUSE_IOCTL_32BIT,
                    cmd: 0x4004_5565,
                    arg: 0x130,
                    in_size: 2,
                    out_size: 0,
                    data: &[7, 8]
                }
            ))
        );
        // fuse_poll_in: fh, kh, flags, events.
        let poll = [&le64(9)[..], &le64(5), &le32(1), &le32(1)].concat();
        assert_eq!(
            parse(&request(FUSE_POLL, 5, &poll)),
            Some((
                5,
                Request::Poll {
                    fh: 9,
                    kh: 5,
                    flags: 1
                }
            ))
        );
        let release = [&le64(9)[..], &[0; 16]].concat();
        assert_eq!(
            parse(&request(FUSE_RELEASE, 6, &release)),
            Some((6, Request::Release { fh: 9 }))
        );
        assert_eq!(
            parse(&request(FUSE_INTERRUPT, 7 | 1, &le64(6))),
            Some((7, Request::Interrupt { unique: 6 }))
        );
        assert_eq!(
            parse(&request(FUSE_OPEN, 8, &[le32(2), le32(0)].concat())),
            Some((8, Request::Open { flags: 2 }))
        );
        assert_eq!(
            parse(&request(46, 9, &[0; 24])),
            Some((9, Request::Other(46)))
        );
        let mut short = request(FUSE_READ, 2, &read);
        short.truncate(60);
        assert_eq!(parse(&short), None, "the header's length disagrees");
        assert_eq!(
            parse(&request(FUSE_READ, 2, &read[..20])),
            None,
            "short body"
        );
    }

    #[test]
    fn replies_carry_the_out_header_and_the_struct() {
        let open = open_reply(0x11, 3);
        assert_eq!(open.len(), OUT_HEADER_LEN + 16, "fuse_open_out");
        assert_eq!(&open[..4], &le32(32));
        assert_eq!(&open[4..8], &0i32.to_ne_bytes());
        assert_eq!(&open[8..16], &le64(0x11));
        assert_eq!(&open[16..24], &le64(3));

        let err = error(0x12, 1);
        assert_eq!(err.len(), OUT_HEADER_LEN);
        assert_eq!(&err[4..8], &(-1i32).to_ne_bytes());

        assert_eq!(write_reply(1, 48).len(), OUT_HEADER_LEN + 8);
        assert_eq!(&poll_reply(1, 0x41)[16..20], &le32(0x41));

        let notify = notify_poll(5);
        assert_eq!(notify.len(), 24);
        assert_eq!(&notify[4..8], &1i32.to_ne_bytes(), "FUSE_NOTIFY_POLL");
        assert_eq!(&notify[8..16], &le64(0), "unique 0 is a notification");
        assert_eq!(&notify[16..], &le64(5));
    }

    #[test]
    fn the_init_reply_is_cuse_init_out_and_the_devname() {
        let init = init_reply(1, 45, 4096, "punktfunk/uinput-pf-seat-1");
        let info = b"DEVNAME=punktfunk/uinput-pf-seat-1\0";
        assert_eq!(init.len(), OUT_HEADER_LEN + 72 + info.len());
        let body = &init[OUT_HEADER_LEN..];
        assert_eq!(&body[..4], &le32(7), "major");
        assert_eq!(&body[4..8], &le32(45), "minor");
        assert_eq!(&body[12..16], &le32(1), "CUSE_UNRESTRICTED_IOCTL");
        assert_eq!(&body[16..24], &[le32(4096), le32(4096)].concat());
        assert_eq!(&body[24..32], &[0; 8], "the kernel picks the device number");
        assert_eq!(&body[72..], info);
    }

    #[test]
    fn an_ioctl_answer_is_its_result_or_a_retry_with_iovecs() {
        let done = ioctl_reply(1, 0, &[1, 2, 3, 4]);
        assert_eq!(done.len(), OUT_HEADER_LEN + 16 + 4);
        assert_eq!(&done[16..32], &[0; 16], "result 0, no flags, no iovecs");
        let retry = ioctl_retry(2, &[(0x1000, 96)], &[(0x1000, 96)]);
        assert_eq!(retry.len(), OUT_HEADER_LEN + 16 + 32);
        let body = &retry[OUT_HEADER_LEN..];
        assert_eq!(
            &body[..16],
            &[le32(0), le32(FUSE_IOCTL_RETRY), le32(1), le32(1)].concat()
        );
        assert_eq!(
            &body[16..],
            &[le64(0x1000), le64(96), le64(0x1000), le64(96)].concat()
        );
    }
}
