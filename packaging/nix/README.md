# punktfunk on NixOS / Nix

First-class Nix support via the repo's `flake.nix`: reproducible builds of the streaming **host**
and the native Linux **client**, a **NixOS module** that wires up everything the RPM/deb do
(systemd user service, udev rules, kernel modules, sysctl tuning, firewall, `input` group), and a
**dev shell** with the pinned toolchain and every system library.

> **Platform:** `x86_64-linux` only (the host encodes with desktop NVENC; matches the RPM's
> `ExclusiveArch: x86_64`). NixOS **24.11 or newer** for the `hardware.graphics` option.

---

## What the flake provides

| Output | Contents |
| --- | --- |
| `packages.x86_64-linux.punktfunk-host` | `punktfunk-host` + `punktfunk-tray` (built with `nvenc` + `vulkan-encode`, like CI) |
| `packages.x86_64-linux.punktfunk-client` | `punktfunk-client` (GTK4 shell) + `punktfunk-session` (Vulkan streamer, without the Skia OSD — see caveats) |
| `packages.x86_64-linux.default` | = `punktfunk-host` |
| `nixosModules.default` | `services.punktfunk.host` / `services.punktfunk.client` |
| `devShells.x86_64-linux.default` | pinned Rust (from `rust-toolchain.toml`) + all build deps |
| `apps` / `checks` / `formatter` | `nix run`, `nix flake check`, `nix fmt` |

One binary per GPU vendor: NVENC/CUDA entry points are `dlopen`'d at runtime, so the host runs on
NVIDIA (zero-copy dmabuf → CUDA → NVENC), AMD/Intel (raw Vulkan-Video HEVC / VAAPI), or software.

---

## Quick start (no NixOS required)

```sh
# Build
nix build git+https://git.unom.io/unom/punktfunk#punktfunk-host
nix build git+https://git.unom.io/unom/punktfunk#punktfunk-client

# Run
nix run git+https://git.unom.io/unom/punktfunk#punktfunk-host -- serve --gamestream
nix run git+https://git.unom.io/unom/punktfunk#punktfunk-client
```

GPU drivers are resolved at runtime from `/run/opengl-driver/lib`. On non-NixOS distros use
[nixGL](https://github.com/nix-community/nixGL) so that path is populated (`nixGL nix run …`); on
NixOS the module (below) sets `hardware.graphics.enable = true` for you.

---

## NixOS module

Add the flake and enable the host and/or client:

```nix
{
  inputs.punktfunk.url = "git+https://git.unom.io/unom/punktfunk";
  # (optional) share your nixpkgs: inputs.punktfunk.inputs.nixpkgs.follows = "nixpkgs";

  outputs = { self, nixpkgs, punktfunk, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        punktfunk.nixosModules.default
        ({ ... }: {
          services.punktfunk.host = {
            enable = true;
            users = [ "alice" ];        # → added to the `input` group for virtual gamepads
            openFirewall = true;        # native + GameStream ports
            settings = {
              PUNKTFUNK_VIDEO_SOURCE = "virtual";
              RUST_LOG = "info";
              # PUNKTFUNK_444 = true;   # booleans render as 1/0
            };
          };

          # …and/or the client on the same or another box:
          services.punktfunk.client = {
            enable = true;
            openFirewall = true;        # UDP 5353 for mDNS discovery
          };
        })
      ];
    };
  };
}
```

Then, in your graphical session:

```sh
systemctl --user enable --now punktfunk-host
```

### Options

`services.punktfunk.host`:

| Option | Default | Meaning |
| --- | --- | --- |
| `enable` | `false` | Install the host + wire udev/sysctl/kernel-modules/firewall and the user service. |
| `gamestream` | `true` | `serve --gamestream` (Moonlight-compatible). `false` = native-only, more secure. |
| `autoStart` | `false` | Add the user service to `default.target` (appliance mode — pair with lingering). |
| `users` | `[ ]` | Users added to the `input` group (virtual gamepads). |
| `settings` | `{ }` | `host.env` key/values (see `${package}/share/punktfunk-host/host.env.example`). |
| `environmentFile` | `null` | Extra `EnvironmentFile` for secrets (e.g. `PUNKTFUNK_MGMT_TOKEN`); loaded optionally. |
| `openFirewall` | `false` | Open the inbound ports (see below). |
| `package` | flake's | Override the package. |

`services.punktfunk.client`: `enable`, `openFirewall` (UDP 5353), `package`.

### What the host module configures for you

Everything the RPM's `%install` + `%post` do, declaratively:

- **systemd `--user` service** `punktfunk-host` → `serve [--gamestream]`, `EnvironmentFile` from
  `settings` (+ optional secret file), `Restart=on-failure`.
- **udev rules** (`60-punktfunk.rules`): `/dev/uinput` + `/dev/uhid` group access and the vhci
  sysfs perms for the virtual Steam Deck.
- **kernel modules**: `uinput`, `uhid`, `vhci-hcd` (usbip transport so Steam Input adopts the
  virtual Deck).
- **sysctl**: `net.core.{r,w}mem_max = 32 MB` (high-bitrate UDP headroom; `mkDefault`).
- **`input` group** membership for `users`.
- **`hardware.graphics.enable = true`** (`mkDefault`) so `/run/opengl-driver/lib` has the driver
  libs the binaries `dlopen`.
- **firewall** (when `openFirewall`): native UDP 9777/5353 + TCP 47990; with `gamestream` also TCP
  47984/47989/48010 + UDP 47998/47999/48000. The media data plane is an ephemeral, hole-punched
  UDP port — nothing fixed to open.
- **tray autostart** entry (`--autostart`; self-gates to users who actually run a host).

### GPU drivers (out of scope of the module — set these yourself)

- **NVIDIA:** `hardware.nvidia` + `hardware.graphics.enable = true`. NVENC/CUDA come from the
  driver at runtime (nothing pinned in the closure).
- **AMD/Intel:** `hardware.graphics.enable = true` with `extraPackages = [ vaapiVdpau … ]` /
  `intel-media-driver` for VAAPI encode; the host's raw Vulkan-Video HEVC path needs only Mesa.

### Headless / appliance

Set `autoStart = true`, enable lingering, and pick a backend in `settings`:

```nix
services.punktfunk.host = {
  enable = true;
  autoStart = true;
  users = [ "streamer" ];
  settings = { PUNKTFUNK_COMPOSITOR = "gamescope"; };  # or kwin/mutter/wlroots
};
users.users.streamer.linger = true;
# For the gamescope/KWin backends extend the service PATH, e.g.:
# systemd.user.services.punktfunk-host.path = [ pkgs.gamescope ];
```

The `${package}/share/punktfunk-host/headless/` helpers (KDE/Sway session scripts, example
`host.env` files, the OpenAPI doc) are installed for reference.

---

## Development

```sh
nix develop        # pinned toolchain (rust-toolchain.toml) + all system libs
cargo build --release -p punktfunk-host -p punktfunk-client-linux -p punktfunk-client-session -p punktfunk-tray
```

The shell exports `PF_FFVK_VULKAN_INCLUDE` (Vulkan headers for pf-ffvk bindgen) and an
`LD_LIBRARY_PATH` that includes `/run/opengl-driver/lib` so `cargo run` finds the GPU driver.
`nix fmt` formats the `.nix` files.

---

## Notes & caveats

- **Build tool:** [crane](https://github.com/ipetkov/crane). The lockfile carries
  `windows 0.62.2` from both crates.io *and* a pinned `microsoft/windows-rs` git rev (the Windows
  client), which `rustPlatform.importCargoLock` can't vendor (colliding `name-version`); crane
  vendors per-source and fetches the git rev via `builtins.fetchGit` (no output hash to maintain).
  Those crates are `cfg(windows)`-gated — vendored, never compiled on Linux.
- **First build compiles from scratch** (no split dep cache — pyrowave-sys builds a CMake tree in
  its build.rs that a crane "dummy" source would drop) and has no public binary cache, so expect a
  long initial build. `nix develop` gives incremental rebuilds.
- **Commit `flake.lock`:** it pins the input revisions (nixpkgs / crane / rust-overlay). It is
  generated on first eval and checked in.
- **Session Skia OSD is off under Nix.** `punktfunk-session`'s default `ui` feature draws its
  on-screen stats/console overlay with `skia-safe`, whose build *downloads* a prebuilt Skia from
  the rust-skia releases — which Nix's network-less build sandbox forbids, and a from-source Skia
  build pulls the whole gn/ninja/python toolchain plus network-fetched third-party. The feature is
  explicitly droppable ("same streaming, stats on stdout only"), so the Nix build compiles the
  session with `--no-default-features --features pyrowave`. **Everything streams**; only the
  session binary's *optional* on-glass stats overlay is absent, and the **GTK shell
  (`punktfunk-client`) is skia-free and fully featured.** Re-adding it means teaching skia-bindings
  to consume a prebuilt Skia offline (a fixed-output derivation of the rust-skia tarball) or a
  vendored from-source Skia build — a tracked follow-up.

## Verified

Both packages build, install, and run on real Nix hardware (NixOS-equivalent: CachyOS + Nix,
RTX 5070 Ti, driver 610). `punktfunk-host --version` and `punktfunk-session` run; the driver
RUNPATH (`/run/opengl-driver/lib`) and the GTK GApps wrapper (GSettings schemas + pixbuf loaders)
are present. Fixes discovered during that bring-up: `CMAKE_POLICY_VERSION_MINIMUM=3.5` (CMake ≥ 4),
system `libopus` (audiopus_sys), and the session Skia note above.
