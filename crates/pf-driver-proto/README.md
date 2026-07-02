# pf-driver-proto

The shared **host ↔ driver binary contract** for punktfunk's Windows **pf-vdisplay** virtual display —
the control IOCTLs and the IDD-push frame transport, defined exactly once.

It's a path dependency of **both** the host workspace ([`crates/punktfunk-host`](../punktfunk-host))
and the out-of-workspace driver workspace ([`packaging/windows/drivers/`](../../packaging/windows/drivers)),
so it must resolve identically from either build graph. That's why it's deliberately self-contained:
`no_std` (+ alloc), platform-neutral (GUID/LUID are plain integers each side converts to its own OS
type), and free of `*.workspace = true` inheritance.

Defining every wire struct here — with `const` size/offset asserts and `bytemuck` round-trips — turns
host↔driver ABI drift into a **compile error** instead of a silent frame or IOCTL corruption.

See the crate root ([`src/`](src/)) for the wire types; the Windows virtual-display design is in
[`design/windows-virtual-display-rust-port.md`](../../design/windows-virtual-display-rust-port.md).
