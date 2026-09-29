"""Offline checks for the Raspberry Pi OS kiosk image."""
import json
import hashlib
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
release = (root / "etc/punktfunk-kiosk-release").read_text().splitlines()
assert f"RELEASE_TAG={sys.argv[2]}" in release
assert f"PUNKTFUNK_VERSION={sys.argv[2].removeprefix('v')}" in release
assert f"FORK_COMMIT={sys.argv[3]}" in release
assert (root / "opt/punktfunk-rpi5/build-version").read_text().strip() == sys.argv[2].removeprefix("v")
assert (root / "opt/punktfunk-rpi5/fork-commit").read_text().strip() == sys.argv[3]
weston = root / "usr/lib/aarch64-linux-gnu/libweston-14/drm-backend.so"
assert hashlib.sha256(weston.read_bytes()).hexdigest() == "9de49e163c8396e4de8a9dc6b7d747e978cada844553c1a951d8fa2707e7f91f", "Missing verified Weston cursor fix"
assert weston.with_name(weston.name + ".punktfunk-original").is_file()
assert "Environment=WESTON_DRM_DISABLE_CURSOR_PLANE=1" in (root / "etc/systemd/system/punktfunk-weston.service").read_text()
unit = (root / "etc/systemd/system/punktfunk-kiosk.service").read_text()
for value in ("PUNKTFUNK_DECODER=v4l2-request", "PUNKTFUNK_PRESENT_MODE=immediate",
              "PUNKTFUNK_SWAPCHAIN_IMAGES=8", "--browse --fullscreen"):
    assert value in unit, f"Missing launch setting: {value}"
assert "HOME=/data" in (root / "etc/default/punktfunk").read_text().splitlines()
settings = json.loads((root / "data/.config/punktfunk/client-gtk-settings.json").read_text())
assert settings["codec"] == "hevc"
binary = (root / "opt/punktfunk-rpi5/punktfunk-session").read_bytes()
assert b"v4l2-request" in binary
assert b"Broadcom GPU: cached backdrop with native-resolution controls" in binary
assert "repaint-window=12" in (root / "etc/xdg/weston/weston.ini").read_text()
assert (root / "usr/lib/aarch64-linux-gnu/dri/v3d_dri.so").is_file(), "Weston needs the Pi OpenGL driver as well as Vulkan"
assert (root / "boot/firmware/kernel_2712.img").is_file()
assert (root / "boot/firmware/bcm2712-rpi-5-b.dtb").is_file()
assert list((root / "usr/lib/modules").glob("*2712/**/rpi-hevc-dec.ko*")), "Pi kernel lacks the HEVC module"
assert (root / "etc/systemd/user/pipewire.service").readlink() == pathlib.Path("/dev/null"), "Avoid a second audio server from the PAM login session"
assert (root / "etc/systemd/system/graphical.target.wants/punktfunk-session.target").is_symlink()
for unit_name in ("userconfig.service", "getty@tty1.service"):
    assert (root / "etc/systemd/system" / unit_name).readlink() == pathlib.Path("/dev/null")
for name in ("ssh.service", "sshd.service", "ssh.socket"):
    assert (root / "etc/systemd/system" / name).readlink() == pathlib.Path("/dev/null")
for home in (root / "root", root / "home", root / "data"):
    assert not list(home.rglob("authorized_keys*")), "Public images must not ship authorized keys"
assert not (root / "home/debug").exists()
for entry in (root / "etc/shadow").read_text().splitlines():
    assert entry.split(":")[1].startswith(("!", "*")), "Public image contains an unlocked password"
assert not list((root / "etc/ssh").glob("ssh_host_*_key*")), "Host keys must be generated per device"
assert (root / "usr/local/bin/punktfunk-diagnostics").is_file()
assert "LD_LIBRARY_PATH=/opt/punktfunk-rpi5/lib" in (root / "usr/local/bin/punktfunk-diagnostics").read_text()
assert "LimitNICE=-10" in (root / "etc/systemd/system/punktfunk-kiosk.service.d/runtime.conf").read_text()
assert "TimeoutStartSec=60" in (root / "etc/systemd/system/punktfunk-clock-ready.service").read_text()
assert b"query SAND transfer formats" in binary, "Missing adaptive 8/10-bit transfer fix"
assert b"Bluetooth management agent registered" in binary, "Missing native Bluetooth manager"
print("Pi OS kiosk: decoder, HEVC, boot assets and startup configuration OK")
