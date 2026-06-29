# Virtual Steam Deck via USB gadget — true Steam Input recognition

**Proven on a real Steam Deck (SteamOS 3.8.11), 2026-06-29.** A `raw_gadget` userspace emulator of a
real 3-interface USB Steam Deck (`28DE:1205`) — mouse = interface 0, keyboard = 1, **controller =
interface 2** — bound to a `dummy_hcd` loopback UDC, so the host's own Steam sees a genuine
interface-2 Deck and **promotes it through Steam Input** (XInput pad emission, glyphs, bindings).

## Why this exists (the interface-2 wall)

A virtual Deck created via **UHID** (the `inject/proto/steam_proto.rs` / `steam_controller.rs` path)
binds the kernel `hid-steam` driver, but **Steam Input will not manage it**: Steam filters the Deck's
controller to USB **interface 2**, and a UHID device has no USB interface number (`Interface: -1` in
Steam's `controller.txt`), so Steam enumerates it but never promotes it. A single-interface DualSense
is accepted at `-1` (no ambiguity), but the multi-interface Deck specifically needs interface 2. See
`design/steam-controller-deck-support.md` §11.

A real multi-interface USB device with the controller on interface 2 requires a **USB gadget**.
SteamOS ships every piece (`CONFIG_USB_DUMMY_HCD=m`, `CONFIG_USB_RAW_GADGET=m`,
`CONFIG_USB_CONFIGFS_F_HID=y`), so this runs on a Deck with no module-building.

## What's here

- **`deck_raw_gadget.c`** — the working emulator. Presents the 3-interface Deck with descriptors
  captured verbatim from a physical Deck (incl. the real 38-byte controller report descriptor), and
  — crucially — answers **every** control transfer, including the HID feature reports (`f_hid` can't,
  so it produced a "zombie controller" in Steam). Streams the 64-byte state report on the interface-2
  interrupt-IN endpoint. Build static (the Deck has no compiler):
  ```sh
  gcc -O2 -static -pthread -o deck_raw_gadget deck_raw_gadget.c
  ```
  Run as root with `dummy_hcd` + `raw_gadget` loaded: `./deck_raw_gadget [seconds]`.
- **`configfs_gadget_up.sh` / `_down.sh`** — the simpler **configfs `f_hid`** variant. It proves the
  structure (interface 2 → `hid-steam` binds → Steam *opens* it + *reserves an XInput slot*) but
  `f_hid` cannot serve HID feature reports, so Steam can't read controller details and drops it as a
  zombie. Kept as the minimal reproducer of the interface-2 result.

## Result (raw_gadget, live)

```
hid-steam ... Steam Controller 'PFDECK000' connected
input: Steam Deck / Steam Deck Motion Sensors            (kernel gamepad + IMU evdevs)
controller.txt: Interface: 2 ... device opened for index 14 ... reserving XInput slot 1
input: Microsoft X-Box 360 pad 1                          (Steam Input's XInput output — promoted)
```
Stable (1 connect, 0 disconnects), no zombie. The kernel `"Steam Deck"` evdev is then grabbed by
Steam Input, which exposes its own X-Box 360 pad — exactly a real Deck's behaviour.

## Key implementation gotchas (all real, all cost time)

- `struct usb_endpoint_descriptor` (ch9.h) is **9 bytes** (audio `bRefresh`/`bSynchAddress`); the wire
  descriptor needs **7** — use a packed 7-byte struct in the config blob or the kernel mis-parses it.
- raw_gadget EP0: a **no-data OUT** control (`SET_CONFIGURATION`, `SET_INTERFACE`, `SET_IDLE`,
  `SET_PROTOCOL`) is completed with a zero-length **`EP0_READ`**, not `EP0_WRITE` (using write →
  `EBUSY`/`can't set config error -110`). IN controls (`GET_*`) use `EP0_WRITE`.
- Don't start the input streamer until after `SET_CONFIGURATION` is fully acked, or its blocking
  `EP_WRITE` starves the control path.
- `dummy_hcd` + `raw_gadget` must both be loaded and `/dev/raw-gadget` present before launch.

## Status / next

Recognition is proven. Remaining: feed real client state (the `steam_proto` serializer already
produces correct Deck reports) through the interface-2 endpoint, and wrap this as a host gamepad
backend (a `raw_gadget` alternative to the UHID `SteamDeckPad`) — SteamOS-host only, since it needs
`dummy_hcd` + `raw_gadget`.
