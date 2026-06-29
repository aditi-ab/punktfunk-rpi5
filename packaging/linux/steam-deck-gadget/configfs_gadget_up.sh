#!/bin/bash
# PoC: stand up a REAL 3-interface USB Steam Deck (28DE:1205) on a dummy_hcd loopback UDC, with the
# controller on **interface 2** (kbd=0, mouse=1) — the structure Steam's controller driver filters
# for. Run as root on the Deck (which ships dummy_hcd + configfs f_hid). Then we check: does hid-steam
# bind interface 2, and does the Deck's own Steam promote it (controller.txt "Interface: 2")?
set -e
G=/sys/kernel/config/usb_gadget/pfdeck

echo "== modprobe dummy_hcd + libcomposite =="
modprobe dummy_hcd
modprobe libcomposite
UDC=$(ls /sys/class/udc | grep -i dummy | head -1)
echo "dummy UDC: ${UDC:-<none found!>}"
[ -n "$UDC" ] || { echo "no dummy UDC — abort"; exit 1; }

# Tear down a prior instance if present.
if [ -d "$G" ]; then
  echo "" > "$G/UDC" 2>/dev/null || true
  for l in "$G"/configs/c.1/hid.usb*; do [ -e "$l" ] && rm -f "$l"; done
  rmdir "$G"/configs/c.1/strings/0x409 2>/dev/null || true
  rmdir "$G"/configs/c.1 2>/dev/null || true
  rmdir "$G"/functions/hid.usb* 2>/dev/null || true
  rmdir "$G"/strings/0x409 2>/dev/null || true
  rmdir "$G" 2>/dev/null || true
fi

echo "== build gadget $G =="
mkdir -p "$G"; cd "$G"
echo 0x28de > idVendor
echo 0x1205 > idProduct
echo 0x0110 > bcdDevice
echo 0x0200 > bcdUSB
mkdir -p strings/0x409
echo "Valve Software"        > strings/0x409/manufacturer
echo "Steam Deck Controller" > strings/0x409/product
echo "PFDECK0001"            > strings/0x409/serialnumber

# --- interface 0: boot keyboard ---
mkdir -p functions/hid.usb0
echo 1 > functions/hid.usb0/protocol
echo 1 > functions/hid.usb0/subclass
echo 8 > functions/hid.usb0/report_length
printf '\x05\x01\x09\x06\xa1\x01\x05\x07\x19\xe0\x29\xe7\x15\x00\x25\x01\x75\x01\x95\x08\x81\x02\x95\x01\x75\x08\x81\x03\x95\x05\x75\x01\x05\x08\x19\x01\x29\x05\x91\x02\x95\x01\x75\x03\x91\x03\x95\x06\x75\x08\x15\x00\x25\x65\x05\x07\x19\x00\x29\x65\x81\x00\xc0' > functions/hid.usb0/report_desc

# --- interface 1: boot mouse ---
mkdir -p functions/hid.usb1
echo 2 > functions/hid.usb1/protocol
echo 1 > functions/hid.usb1/subclass
echo 4 > functions/hid.usb1/report_length
printf '\x05\x01\x09\x02\xa1\x01\x09\x01\xa1\x00\x05\x09\x19\x01\x29\x03\x15\x00\x25\x01\x95\x03\x75\x01\x81\x02\x95\x01\x75\x05\x81\x03\x05\x01\x09\x30\x09\x31\x15\x81\x25\x7f\x75\x08\x95\x02\x81\x06\xc0\xc0' > functions/hid.usb1/report_desc

# --- interface 2: the Steam Deck controller (STEAMDECK_RDESC) ---
mkdir -p functions/hid.usb2
echo 0  > functions/hid.usb2/protocol
echo 0  > functions/hid.usb2/subclass
echo 64 > functions/hid.usb2/report_length
printf '\x06\x00\xff\x09\x01\xa1\x01\x15\x00\x26\xff\x00\x75\x08\x95\x40\x09\x01\x81\x02\x09\x01\x95\x40\xb1\x02\xc0' > functions/hid.usb2/report_desc

# --- config, link in order so interface numbers are 0,1,2 ---
mkdir -p configs/c.1/strings/0x409
echo "Punktfunk virtual Deck" > configs/c.1/strings/0x409/configuration
echo 250 > configs/c.1/MaxPower
ln -s functions/hid.usb0 configs/c.1/
ln -s functions/hid.usb1 configs/c.1/
ln -s functions/hid.usb2 configs/c.1/

echo "== bind to $UDC =="
echo "$UDC" > UDC
sleep 2

echo ""; echo "===== VERIFY ====="
echo "--- /sys hid devices for 28DE (which interface, which driver) ---"
for d in /sys/bus/hid/devices/*28DE*; do
  [ -e "$d" ] || continue
  rp=$(readlink -f "$d")
  echo "  $(basename "$d"): bInterfaceNumber=$(cat "$rp/../bInterfaceNumber" 2>/dev/null) driver=$(basename "$(readlink -f "$d/driver" 2>/dev/null)")"
done
echo "--- hidg char devices (controller = hidg for interface 2) ---"; ls -1 /dev/hidg* 2>/dev/null
echo "--- kernel log (hid-steam bind + Steam Deck evdev) ---"
journalctl -k --since "20 seconds ago" --no-pager 2>/dev/null | grep -iE "steam|28de|1205|hid-generic" | tail -10
echo "--- /proc input: Steam Deck evdevs created? ---"
grep -c '^N: Name="Steam Deck' /proc/bus/input/devices | sed 's/^/  Steam Deck input nodes: /'
echo "--- lsusb ---"; lsusb -d 28de:1205 2>/dev/null || true
echo ""
echo "Gadget is UP. Feed a neutral controller report with:  printf '\\x01\\x00\\x09\\x3c' | dd of=/dev/hidg2 ..."
echo "Tear down with: deck_gadget_down.sh"
