#!/usr/bin/env python3
"""Drive the kiosk's existing keyboard controls through a temporary uinput device."""

import argparse
import os
import struct
import time

KEYS = {
    "up": 103, "down": 108, "left": 105, "right": 106,
    "enter": 28, "escape": 1, "tab": 15, "home": 102, "end": 107,
}
SHORTCUTS = {"disconnect": 32, "stats": 31, "ring": 24}
MODIFIERS = (29, 56, 42)  # Ctrl, Alt, Shift.


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dry-run", action="store_true")
    commands = parser.add_subparsers(dest="action", required=True)
    tap = commands.add_parser("tap")
    tap.add_argument("key", choices=KEYS)
    tap.add_argument("--count", type=int, default=1)
    shortcut = commands.add_parser("shortcut")
    shortcut.add_argument("key", choices=SHORTCUTS)
    scroll = commands.add_parser("scroll")
    scroll.add_argument("--seconds", type=float, default=20)
    args = parser.parse_args()
    if args.action == "tap" and not 1 <= args.count <= 100:
        parser.error("count must be between 1 and 100")
    if args.action == "scroll" and not 0 < args.seconds <= 60:
        parser.error("seconds must be between 0 and 60")
    return args


class Keyboard:
    def __enter__(self):
        import fcntl
        self.ioctl = fcntl.ioctl
        self.fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
        self.held = set()
        try:
            self.ioctl(self.fd, 0x40045564, 1)  # UI_SET_EVBIT(EV_KEY).
            for key in range(1, 249):
                self.ioctl(self.fd, 0x40045565, key)
            device = struct.pack("80sHHHHI", b"Punktfunk diagnostic keyboard", 6, 0, 0, 1, 0)
            os.write(self.fd, device + bytes(64 * 4 * 4))
            self.ioctl(self.fd, 0x5501)  # UI_DEV_CREATE.
            time.sleep(1)  # Let udev and Weston discover the temporary device.
            return self
        except BaseException:
            os.close(self.fd)
            raise

    def event(self, key, pressed):
        os.write(self.fd, struct.pack("llHHi", 0, 0, 1, key, int(pressed)))
        os.write(self.fd, struct.pack("llHHi", 0, 0, 0, 0, 0))
        if pressed:
            self.held.add(key)
        else:
            self.held.discard(key)

    def tap(self, key, modifiers=()):
        try:
            for modifier in modifiers:
                self.event(modifier, True)
            self.event(key, True)
            time.sleep(0.06)
        finally:
            self.event(key, False)
            for modifier in reversed(modifiers):
                self.event(modifier, False)

    def __exit__(self, *_):
        try:
            for key in tuple(self.held):
                self.event(key, False)
            self.ioctl(self.fd, 0x5502)  # UI_DEV_DESTROY.
        finally:
            os.close(self.fd)


def main():
    args = arguments()
    if args.dry_run:
        print(vars(args))
        return
    if os.geteuid() != 0:
        raise SystemExit("Run with sudo to access /dev/uinput.")
    with Keyboard() as keyboard:
        if args.action == "shortcut":
            keyboard.tap(SHORTCUTS[args.key], MODIFIERS)
        elif args.action == "tap":
            for _ in range(args.count):
                keyboard.tap(KEYS[args.key])
                time.sleep(0.15)
        else:
            started = time.monotonic()
            while time.monotonic() - started < args.seconds:
                direction = "down" if int((time.monotonic() - started) / 3) % 2 == 0 else "up"
                keyboard.tap(KEYS[direction])
                time.sleep(0.07)
    print("Input sequence completed.")


if __name__ == "__main__":
    main()
