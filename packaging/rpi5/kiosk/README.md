# Raspberry Pi 5 kiosk image

The release workflow builds the portable bundle and complete Raspberry Pi OS Lite
image from the same Git tag. Every image checks the bundle version and commit
against that tag. Images boot directly into Punktfunk under Weston, store settings
in `/data`, and use wired DHCP. There is no desktop or provisioned login account.
SSH and password login are disabled; no workstation keys are embedded.

Download `punktfunk-<release>-kiosk.img.xz` and its `.img.sha256` file from
the matching GitHub release. Verify with `sha256sum -c <checksum-file>` after also
downloading the package inventory and build manifest named in that file. Flash the
compressed image with Raspberry Pi Imager. This erases the selected target drive.
The root filesystem expands on first boot.

## Build

On native ARM64 Linux, check out the release tag, build the portable bundle with
`packaging/rpi5/build-release.sh`, then run:

```sh
sudo apt-get install curl xz-utils fdisk util-linux e2fsprogs dosfstools python3
sudo bash packaging/rpi5/kiosk/build-image.sh "$TAG" \
  "dist/punktfunk-${TAG#v}-linux-arm64.tar.gz" dist
```

An x86-64 Linux or WSL image builder also needs `qemu-user-static` and registered
AArch64 binfmt support. The portable bundle itself is built on native ARM64.
The builder needs loop devices, mount privileges, network access and about 12 GB
of free disk space. `PUNKTFUNK_IMAGE_CACHE` overrides the base-image cache location.

The base image is pinned by URL and SHA-256. Weston packages are pinned to the ABI
of the included patched DRM module; a checksum verifies that module before install.
`build-weston-cursor-backend.sh` rebuilds from a checksum-pinned upstream source
archive; `WESTON-COPYING` contains its license. Its patch disables hardware cursor planes when the compositor environment requests
it. Upgrading Weston requires rebuilding and validating the module first.
The installed Weston package retains its copyright notices in `/usr/share/doc`;
the image also includes the local patch and module checksum there.

The builder checks service configuration, dependencies, firmware, decoder markers,
remote-login defaults and available space. It produces the compressed image,
checksums, package inventory and build manifest. A tag push starts the workflow;
manual dispatch retries an existing tag. Assets go to that tag's release, creating
a draft if needed. Release notes must distinguish offline build checks from physical
Pi 5 testing. Boot, controller navigation and sustained HEVC playback remain
unverified until tested on hardware; an image may be published with that limitation.

## VirtualHere USB sharing

Under **Settings > Raspberry Pi > VirtualHere**, choose **Download and install**.
The kiosk downloads the generic ARM64 server directly from VirtualHere over HTTPS
and checks its pinned SHA-256 before installation. No VirtualHere binary or license
is redistributed in the image. If the vendor replaces the download, installation
stops with an update message instead of running an unreviewed binary.

Turn **USB sharing** on after installation. Sharing stays off until enabled and
then starts automatically on reboot. Install VirtualHere Client on the gaming PC;
it discovers the Pi on the same LAN. For routed networks, add the displayed Pi IP
as a hub in VirtualHere Client (TCP port 7575). Turning sharing off releases devices
and disables automatic startup.

The generic server shares one USB device free of charge. More simultaneous devices
require a VirtualHere license. Free use needs the graphical PC client; running
the client as a service or using the console-only client requires a licensed
server. The optimized Raspberry Pi build requires a license
and is not installed by this action. Follow **License setup** in the Pi menu: in
VirtualHere Client on the PC, right-click **USB Servers** (called **USB Hubs** in older clients),
choose **License > Enter License(s)** and paste the purchased key. The server stores it in `/data/virtualhere/config.ini`,
accessible only to root. **Key saved** confirms storage, not license validity;
VirtualHere Client reports activation errors. Licenses survive reboots and
Punktfunk bundle updates. Keep your purchase email before reflashing the SD card.

Enable the VirtualHere plugin on the Punktfunk host to attach and release selected
USB devices with streaming sessions. For a controller passed through VirtualHere,
turn off Punktfunk's **Forward controllers** to avoid duplicate input. Local Pi
controls can become unavailable while the PC owns that USB device.

Vendor documentation: [server and licensing](https://www.virtualhere.com/usb_server_software),
[license purchase](https://www.virtualhere.com/purchase),
[client download](https://www.virtualhere.com/usb_client_software).
