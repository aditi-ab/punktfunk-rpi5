#!/bin/bash
# Tear down the PoC virtual Deck gadget.
G=/sys/kernel/config/usb_gadget/pfdeck
[ -d "$G" ] || { echo "no gadget"; exit 0; }
echo "" > "$G/UDC" 2>/dev/null || true
for l in "$G"/configs/c.1/hid.usb*; do [ -e "$l" ] && rm -f "$l"; done
rmdir "$G"/configs/c.1/strings/0x409 2>/dev/null || true
rmdir "$G"/configs/c.1 2>/dev/null || true
rmdir "$G"/functions/hid.usb* 2>/dev/null || true
rmdir "$G"/strings/0x409 2>/dev/null || true
rmdir "$G" 2>/dev/null || true
echo "gadget torn down ($(ls /sys/kernel/config/usb_gadget/ 2>/dev/null | wc -l) gadgets remain)"
