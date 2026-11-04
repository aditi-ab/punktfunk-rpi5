# VirtualHere kiosk integration and Bazzite host checks

Source: `be47b1ed993fa7b12d07e61f99a05d2190fe45de` (`v0.46.0-rpi5.28`).
Patch replay tree: `d84597444f942cdb923db39f367720144961e488`.

## Kiosk behavior

Settings > Raspberry Pi > VirtualHere downloads the generic ARM64 server on demand.
The vendor binary is not redistributed. SHA-256:
`09deced5586ed37b8b3db7a6187d84029f65d487c3c2b24a949b993b3d582077`
(VirtualHere server 4.8.8, 445924 bytes). Installation does not enable USB sharing.
The explicit sharing switch enables or disables the system service and its boot
startup. Configuration and license data are root-only under `/data/virtualhere`.
License help uses the vendor's PC client; key presence is not proof of validity.

## Local verification

- 15 Python helper tests passed, including atomic installation, hash mismatch,
  redirect rejection, credential privacy, license preservation and stop timeout.
- Real vendor HTTPS download passed the pinned hash and ARM64 ELF checks.
- ARM64 server help ran under QEMU. In an isolated network namespace, the server
  stayed in the foreground and accepted TCP 7575. A 5-second termination check
  needed SIGKILL; a separate run exited normally within the 20-second deadline.
  The shipped unit bounds stop to 10 seconds, below the helper's 15-second timeout.
- Three appliance UI tests, three core helper tests and all 62 settings tests
  passed in WSL. Rendered VirtualHere and license pages were inspected at 1080p.
- Writing checks, shell syntax and exact downstream patch replay passed.
- Native build 27 compiled successfully but failed two old menu catalog assertions:
  the row count and Android exclusion list omitted VirtualHere. Those expectations
  were corrected; build 28 contains the same runtime behavior.

## Bazzite host troubleshooting

The user confirmed the plugin became Running after enabling/restarting
`punktfunk-scripting.service`. Two sandbox visibility problems then appeared:

1. On Bazzite, `/usr/local/bin` resolves to `/var/usrlocal/bin`. Grant that real
   directory read access with `punktfunk-host plugins grant virtualhere
   /var/usrlocal/bin`. The plugin then found the client executable.
2. VirtualHere Client 6.0.3 uses `/tmp/vhclient_ipc` for its Unix socket. Plugin
   0.2.0 only declares the older `/tmp/vhclient` and `/tmp/vhclient_response` FIFOs.
   In an isolated network/mount/PID namespace, LIST worked outside bubblewrap but
   returned code 2 with only the FIFOs bound. Binding `/tmp/vhclient_ipc` read-only
   restored LIST successfully. Writable binding also worked but was unnecessary.

The user applied `punktfunk-host plugins grant virtualhere /tmp/vhclient_ipc`
and restarted the runner. Their screenshot confirms green transport, client LIST
and daemon checks. No shared devices or rules were configured at that point.
The host client was launched with `-n`; that alone does not configure boot startup.
The user's LIST output also had Auto-Use All enabled, which can claim devices as
soon as a server appears instead of waiting for plugin rules.

Physical USB passthrough, paid license activation and a fresh image boot remain
unverified. The existing Pi was updated to the verified .28 bundle without reflashing.
Its VirtualHere helper, service and sudoers rules were installed; sharing remains off.
The kiosk is active with zero restarts after deployment.
