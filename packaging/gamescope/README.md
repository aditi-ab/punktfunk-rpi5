# `punktfunk-gamescope` — gamescope with 10-bit HDR PipeWire capture

Upstream gamescope's built-in PipeWire node is SDR-only: `build_format_params()` offers `BGRx`
and `NV12`, and `paint_pipewire()` hardcodes a Gamma-2.2 composite with the SDR screenshot LUT
set. An HDR game therefore reaches every capture consumer already tone-mapped down — which is
why the punktfunk gamescope backend has always streamed 8-bit, even though games *can* render
HDR on a headless gamescope today (`--hdr-enabled --hdr-debug-force-support`).

The patches here add the missing half, and nothing else. See
`punktfunk-planning/design/gamescope-hdr-virtual-output.md` for the full design.

| Patch | What | Upstream? |
|---|---|---|
| `0001-pipewire-offer-10-bit-BT.2020-PQ-capture-formats-HDR.patch` | Offer SPA `xRGB_210LE`/`xBGR_210LE` with MANDATORY SMPTE ST.2084 + BT.2020 props, map them to `DRM_FORMAT_XRGB2101010`/`XBGR2101010`, and composite them with `g_ScreenshotColorMgmtLutsHDR` + `EOTF_PQ` | **Yes** — offered against [gamescope#2126](https://github.com/ValveSoftware/gamescope/issues/2126) |
| `0002-pipewire-optionally-composite-the-cursor-into-the-ca.patch` | `--pipewire-composite-cursor` (off by default): paint the pointer into the capture stream, using the same `MouseCursor::paint` call the scanout composite uses | **Yes** — independently useful to any consumer with no cursor of its own |
| `0003-headless-advertise-the-virtual-display-s-mode-and-re.patch` | Give `CHeadlessConnector` a real `GetModes()` + `GetValidDynamicRefreshRates()` from the resolved `-W`/`-H`/`-r`, report `GAMESCOPE_SCREEN_TYPE_EXTERNAL` so `update_mode_atoms` publishes the list, and add `--custom-refresh-rates` | **Yes** — a headless session that cannot report its own mode is a plain bug |
| `0004-pipewire-optionally-composite-the-external-overlay-i.patch` | `--pipewire-composite-external-overlay` (off by default): paint the external overlay layer (mangoapp — the fps/stats readout) into the capture stream | **Yes** — same shape as the cursor patch, same argument |
| `0005-punktfunk-stamp-the-version-banner-with-pfhdrN.patch` | Append `+pfhdr<N>` to the `--version` banner | **No** — ours only, retired when the functional patches above land upstream |
| `0006-punktfunk-never-destroy-the-Vulkan-device-or-output-.patch` | Give `g_device` and `g_output` storage that is never destroyed, so their destructors cannot call a Vulkan driver glibc has already unloaded at `exit()` | **Yes** — a plain static-destruction-order bug, not punktfunk-specific |
| `0007-pipewire-never-leave-pw_buffer-user_data-pointing-at.patch` | Associate `pw_buffer->user_data` with its `pipewire_buffer` for every path out of `add_buffer`, clear it in `remove_buffer` (the last point both halves are known), and null-check the consumers — killing the use-after-free that aborted the session on every capture renegotiation | **Yes** — a plain use-after-free in the PipeWire buffer lifecycle |

### Why the headless patch matters

A headless gamescope is how a streaming host gives a game a display: the caller passes the
client's exact mode and expects the session to run at it. It *does* — but it never told anyone.
`CHeadlessConnector` returned an empty span from both `GetModes()` and
`GetValidDynamicRefreshRates()` and reported `GAMESCOPE_SCREEN_TYPE_INTERNAL`, so
`update_mode_atoms()` **deleted** `GAMESCOPE_DISPLAY_MODE_LIST_EXTERNAL` (no resolution list) and
`wlserver_send_gamescope_control()` fell through to a **one-entry** refresh list built from
`g_nOutputRefresh` (no refresh list). With `-r` absent that entry is `Init()`'s 60 Hz default, so a
client on a 120 Hz panel was told its display was 60 Hz — and games capped themselves to it. Field
report 2026-08-08: "gamescope only shows 60hz and there's no other option".

### Why the cursor patch matters more than it looks

gamescope keeps the pointer out of its PipeWire node — it lives on a hardware plane for scanout —
so punktfunk has always reconstructed it from XFixes and blended it into every frame host-side.
That blend is what forces the encode path onto its compute colour-conversion arm: the zero-copy
RGB-direct encode source (`VK_VALVE_video_encode_rgb_conversion`) hands the captured buffer to a
fixed-function front end with no blend stage. Painting the cursor into the node removes the reason
for the blend, and with it a full-frame pass per frame — a gamescope session becomes the first one
that can be genuinely zero-copy end to end.

### Why the buffer-lifetime patch is the one that made sessions unusable

Patch 0007 is not a refinement — without it a managed gamescope session dies on essentially every
client connect. The host sets the session to the client's mode, the mode change renegotiates the
PipeWire stream, and the renegotiation is exactly what trips upstream's dangling
`pw_buffer->user_data`. The signature to recognise:

```
punktfunk-gamescope: ../src/pipewire.cpp:88: void destroy_buffer(pipewire_buffer*):
  Assertion `false' failed.
#5  destroy_buffer(pipewire_buffer*).cold
```

It is a use-after-free wearing an `assert(false); // unreachable` as a disguise — `buffer->type`
is read out of freed memory and falls off the end of the `switch`. A zeroed slot gives the SIGSEGV
variant of the same fault instead. Two traps when triaging it:

* **It is not HDR-specific.** The abort was first seen right after a 10-bit stream negotiated, so
  it looked like the HDR path and `PUNKTFUNK_GAMESCOPE_HDR=0` looked like a workaround. It is not
  — the same abort reproduces on an SDR session with no `--hdr-enabled` in the command line.
  Check the failing process's actual argv before believing an HDR association.
* **`gamescope-session-plus` hides it.** When our binary crash-loops, the session script retries
  and eventually comes up on the *stock* `/usr/bin/gamescope` at its default 1920×1080. So the box
  lands in a working-looking game mode at the wrong resolution and without any of these patches.
  Read the banner in `~/.gamescope-stdout.log`, not the fact that a session exists.

## Why the marker exists

punktfunk decides a session's shape **before** the virtual display exists: the bit depth at
handshake time (irrevocable — a PQ stream handed to an 8-bit encoder is a deliberate hard error),
and whether the host must composite the cursor before the encoder is even opened. Both answers
must therefore be static properties of the resolved binary, not optimistic negotiations. The host
runs `<gamescope> --version` once per boot and reads the revision — see `gamescope_patch_level()`
in `crates/pf-vdisplay/src/vdisplay/linux/gamescope/discovery.rs`.

The number is a **monotonic patch-set revision**, so one probe answers every capability:

| Level | Adds |
|---|---|
| `+pfhdr1` | 10-bit BT.2020/PQ capture formats |
| `+pfhdr2` | …and `--pipewire-composite-cursor` |
| `+pfhdr3` | …and the headless connector advertises its mode + `--custom-refresh-rates` |
| `+pfhdr4` | …and `--pipewire-composite-external-overlay` |

Bump it whenever a patch adds or changes something the host must know about before it spawns.

A patch that only fixes a crash does **not** bump it: `0006` (the exit-time Vulkan teardown fix)
changes nothing the host probes for, so the level stays `+pfhdr4` and the rebuild ships as a
`pkgrel` bump instead — exactly the split the PKGBUILD's own comment describes. Bumping the level
for a bugfix would be worse than useless: it would advertise a capability tier that does not exist
and strand hosts that gate on it.

⚠️ The two indirect spawn modes (the `GAMESCOPE_BIN` wrapper for gamescope-session-plus, and the
SteamOS PATH shim) pass these flags through `PF_HDR_ARGS`, so they share one dependency: if the
session ignores `GAMESCOPE_BIN`/`PATH` and execs the distro's gamescope, it gets neither the HDR
formats nor the cursor flag. HDR fails loudly there (the capture negotiation times out and latches
an SDR downgrade) — but a missing cursor would be silent, because the host was told the compositor
would paint the pointer and so painted none itself.

Both managed paths therefore **verify after spawn**: once the session's node appears,
`verify_managed_spawn_flags` reads the running compositor's `/proc/<pid>/cmdline` and refuses the
session if a flag we passed isn't there. The plan is fixed by then (`cursor_blend` feeds the encoder
open, which precedes the display), so the session cannot be corrected in place — instead the
capability is latched off for the process and the spawn fails, and the retry resolves a correct SDR
host-composited session. One rejected attempt per boot, then it converges.

The check fails **open** at every ambiguity: no flags expected, or no readable gamescope in
`/proc`, says nothing. Only a compositor we can see, missing a flag we can name, fails.

## Which binary the host runs

Resolution order, applied identically by the bare spawn, the `GAMESCOPE_BIN` wrapper
(gamescope-session-plus) and the SteamOS PATH shim:

1. `PUNKTFUNK_GAMESCOPE_BIN` — absolute path override
2. `punktfunk-gamescope` on `PATH`
3. `gamescope`

So installing this build under the name `punktfunk-gamescope` is enough; nothing replaces the
distro's `gamescope`.

## Building

Pinned upstream: `5fb8dce4` (master, 2026-08-03 — `3.16.25-11-g5fb8dce`). The patches apply
cleanly to that commit; they touch `src/pipewire.cpp`, `src/steamcompmgr.cpp`,
`src/rendervulkan.cpp`, `src/rendervulkan.hpp` and `src/meson.build` only.

The bump from `8c676c39` is deliberate: it brings upstream's `vulkan_get_rgb10_capture_format()`
(`ff6b924`), which probes `linearTilingFeatures` for STORAGE+SAMPLED and falls back to
`DRM_FORMAT_XBGR2101010` on devices that cannot do linear-tiled `A2R10G10B10` — i.e. every
NVIDIA. That covers the paths that are upstream's rather than ours: the RGB intermediate
`paint_pipewire()` acquires when the stream is YCbCr, and AVIF screenshots. Our own 10-bit RGB
node is covered by patch `0001`, which offers `xBGR_210LE` first for the same reason.

```sh
git clone https://github.com/ValveSoftware/gamescope.git
cd gamescope
git checkout 5fb8dce4
git submodule update --init --recursive          # or let meson fetch the subprojects
git am /path/to/punktfunk/packaging/gamescope/patches/*.patch

meson setup build/ --prefix=/usr -Dpipewire=enabled
ninja -C build/
# install as punktfunk-gamescope, NOT as gamescope
install -Dm755 build/src/gamescope /usr/bin/punktfunk-gamescope
```

### Build dependencies

They are gamescope's, not ours, and they vary by distro. Two shortcuts that work:

```sh
# Fedora / Bazzite (inside a toolbox/distrobox — the host is immutable)
sudo dnf install -y dnf-plugins-core meson ninja-build glslc
sudo dnf builddep -y gamescope
sudo dnf install -y xorg-x11-server-Xwayland-devel      # NOT pulled by builddep; wlroots needs it
sudo dnf install -y libstdc++-static                    # NOT pulled by builddep; see below

# Arch / SteamOS — see the makedepends in ./PKGBUILD
```

⚠️ `dnf builddep gamescope` resolves Fedora's *packaged* gamescope, which is older than the master
we pin, so it can come up short. `xorg-x11-server-Xwayland-devel` is the one that actually bit
(2026-07-28, Fedora 43): without it wlroots' configure fails with `Neither a subproject directory
nor a xserver.wrap file was found`, several minutes into an otherwise clean run. If a different
one surfaces, meson names it — install and re-run with `--srcdir` so the clone is not repeated.

⚠️ `libstdc++-static` is a **punktfunk** requirement, not gamescope's, so no builddep will ever pull
it: the build script links the C++ runtime statically on purpose (see the long comment beside
`LDFLAGS` in `build-punktfunk-gamescope.sh`). Without it meson fails at configure with a message
that names neither the flag nor the package (2026-08-10, Fedora 44):

```
ERROR: Compiler c++ cannot compile programs.
  /usr/bin/ld.bfd: cannot find -lstdc++
```

The cleanup trap deletes the temp checkout on failure, taking `meson-logs/meson-log.txt` with it —
so build with `--srcdir` when diagnosing, or the evidence is gone before you can read it.

`gamescope` needs `CAP_SYS_NICE` for its realtime priority; the distro packages set it on their
own binary. Mirror it if you install ours system-wide:

```sh
setcap 'CAP_SYS_NICE=eip' /usr/bin/punktfunk-gamescope
```

## How each channel ships it

Four packaging paths, one build recipe — `build-punktfunk-gamescope.sh` — because the two things
that are easy to get wrong (which patches get applied, and whether wlroots is linked statically)
must not be decided twice.

| Channel | Built by | Notes |
|---|---|---|
| Bazzite / Fedora Atomic | `.gitea/workflows/rpm.yml` → `build-sysext.sh --gamescope` | Inside the matching Fedora container, per major — the binary is soname-coupled to its base exactly like the RPM |
| Arch / SteamOS | `.gitea/workflows/arch.yml` → `makepkg` on `./PKGBUILD` | Its own pkgbase in the same pacman repo; `pacman -S punktfunk-gamescope` |
| NixOS | `packaging/nix/gamescope.nix` (an `overrideAttrs` on nixpkgs' gamescope) | The one path that does NOT call the script — nixpkgs already solves the submodules, and a nix closure names every library it links |
| Anything else | the script, by hand | See *Building* above |

Both CI builds are **cached on `packaging/gamescope/**`** and **best-effort**. Cached because this
tree depends on nothing else in the repo, so a normal push restores a binary instead of spending
ten minutes on someone else's C++; best-effort because punktfunk works without it (SDR on the
gamescope backend, which is what every release before this one did) and a hiccup building gamescope
must not cost the packages those workflows exist to publish. A failed build emits a `::warning::`
and is never cached, so the next run retries.

Note what is NOT in that table: the `.deb`. Debian/Ubuntu boxes build it by hand for now.

## Verifying the patch on a box (P0 exit)

```sh
punktfunk-gamescope --version                    # must contain +pfhdr4
punktfunk-gamescope --backend headless -W 1920 -H 1080 -r 60 \
    --hdr-enabled --hdr-debug-force-support --pipewire-composite-cursor -- vkcube &
pw-dump | grep -A40 '"gamescope"'                # node offers xRGB_210LE / xBGR_210LE
```

The stream is only 10-bit once a **consumer** asks for it: the formats are listed last, so any
consumer that negotiates the 8-bit stream today keeps negotiating it bit-for-bit.

## Rebase policy

The functional patch is two files and mirrors code that already exists in-tree (the HDR AVIF
screenshot path), so it rebases cheaply. We pin the gamescope commit we ship; when upstream
takes it, both patches are dropped and the host's capability probe becomes a plain version
floor.
