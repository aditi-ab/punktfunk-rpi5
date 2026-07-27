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
| `packages.x86_64-linux.punktfunk-web` | the management web console (bun-built Nitro SSR bundle; SPAKE2 pairing + host status) |
| `packages.x86_64-linux.punktfunk-scripting` | the plugin/script runner (bun-bundled Effect SDK; supervises host automation) |
| `packages.x86_64-linux.default` | = `punktfunk-host` |
| `nixosModules.default` | `services.punktfunk.host` / `.client` / `.web` / `.scripting` |
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

`services.punktfunk.web` (the management console — **on by default whenever the host is enabled**,
mirroring the RPM's `Recommends: punktfunk-web`):

| Option | Default | Meaning |
| --- | --- | --- |
| `enable` | `host.enable` | Run the console as a `systemd --user` service on **TCP 47992 (HTTPS)**. Set `false` for a console-less host. |
| `openFirewall` | `host.openFirewall` | Open TCP 47992 so other devices on the LAN can reach it. |
| `autoStart` | `host.autoStart` | Add the console user service to `default.target` (appliance mode). |
| `package` | flake's | Override the package. |

The console is **auto-wired** to the host on the same box: it reads the host's per-user
`~/.config/punktfunk/{mgmt-token,cert.pem,key.pem}` (written by `serve`), serves HTTPS with the
host's own identity cert, and proxies the loopback mgmt API with the bearer token injected
server-side (never sent to the browser). A login password is generated on first start — read it
with `journalctl --user -u punktfunk-web-init` (or `~/.config/punktfunk/web-password`). Then open
`https://<host-ip>:47992` and trust the self-signed host cert once. Enable it (with the host) via
`systemctl --user enable --now punktfunk-web`.

`services.punktfunk.scripting` (the plugin/script runner — installed with the host, but **opt-in to
run**):

| Option | Default | Meaning |
| --- | --- | --- |
| `enable` | `host.enable` | Install the runner + define its `systemd --user` unit `punktfunk-scripting`. |
| `autoStart` | `false` | Add the unit to `default.target`. Off even on an auto-start host — running operator scripts/plugins is a deliberate opt-in. |
| `package` | flake's | Override the package. |

The runner discovers loose scripts under `~/.config/punktfunk/scripts` and installed
`punktfunk-plugin-*` packages under `~/.config/punktfunk/plugins`, and supervises each as an Effect
fiber (SIGTERM shuts the tree down structurally so plugin finalizers run). A plugin auto-wires to
the host's mgmt token + identity cert. It's inert until you add automation, so the unit ships
un-started; turn it on with `systemctl --user enable --now punktfunk-scripting`.

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

Set `autoStart = true`, enable lingering, and — for a **dedicated single-session appliance** —
pin a backend in `settings` (pinning `PUNKTFUNK_COMPOSITOR` disables live-session auto-detection,
so leave it out on any box that switches between a desktop and Game Mode):

```nix
services.punktfunk.host = {
  enable = true;
  autoStart = true;
  users = [ "streamer" ];
  settings = { PUNKTFUNK_COMPOSITOR = "gamescope"; };  # appliance-only; omit to auto-detect
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
cargo build --release -p punktfunk-host -p punktfunk-client-linux -p punktfunk-client-session
# The tray gets its OWN invocation — co-building it with the host unifies the host's
# ashpd -> zbus/tokio onto the tray's zbus (which runs ksni's async-io executor, no tokio runtime),
# and the resulting binary panics at launch: "there is no reactor running, must be called from the
# context of a Tokio 1.x runtime". Same split the .deb / RPM / Arch packaging does.
cargo build --release -p punktfunk-tray
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
- **The status tray is built in its own derivation, on purpose.** `punktfunk-tray` uses `ksni`'s
  `async-io` zbus executor with no tokio runtime (by design — see `crates/punktfunk-tray/Cargo.toml`).
  Cargo unifies features across everything in one `cargo build`, so co-building the tray with the
  host would pull the host's `ashpd → zbus/tokio` onto the tray's shared `zbus`, and the tray then
  panics at startup (`there is no reactor running, must be called from the context of a Tokio 1.x
  runtime`). Building it as a separate `-p punktfunk-tray` invocation keeps its `zbus` on async-io;
  the host package copies the resulting binary into its `$out`. (The rpm/arch builds split it the same
  way. The **.deb did not**, despite its sibling comments claiming otherwise: `deb.yml` co-built
  `-p punktfunk-host -p punktfunk-tray`, and `build-deb.sh`'s own standalone build was skipped because
  the poisoned artifact already existed — so this shipped as a real crash-at-launch on Debian/Ubuntu,
  not a latent one. Fixed 2026-07-27: the workflow no longer co-builds it and `build-deb.sh` now
  rebuilds it unconditionally.)
- **The bun packages (`punktfunk-web`, `punktfunk-scripting`) use [bun2nix](https://github.com/nix-community/bun2nix).**
  Their `node_modules` is fetched **one `fetchurl` per package**, straight from the integrity hashes
  already in the lockfile, via a generated-and-committed `bun.nix` (`web/bun.nix`, `sdk/bun.nix`).
  There is **no aggregate deps hash to bump** — the previous design put `bun install` in a
  fixed-output derivation whose single `outputHash` silently went stale on every lockfile change and
  broke the build. `bun.nix` regenerates itself: `bun2nix` is a devDependency of both packages and
  runs on every `bun install` (web's `postinstall`; the SDK's `prepare`, since sdk/ is the
  *published* `@punktfunk/host` package and a `postinstall` would then fire on consumers' installs).
  Regenerate by hand with `cd web && bunx bun2nix -o bun.nix` if a lockfile is ever edited directly.
  The `@unom` scope needs no special handling: `web/bun.lock` records those tarballs' full
  `https://git.unom.io/api/packages/unom/npm/…` URLs and the registry is read-public (the same
  anonymous pull CI's rpm/deb builds do).

  > ⚠ **`bun.nix` has no schema stability across bun2nix versions.** The flake input is pinned
  > (`github:nix-community/bun2nix?ref=2.1.2`) and the npm devDependency is pinned to the *same*
  > exact version in `web/package.json` + `sdk/package.json`. Move both together, then rerun
  > `bun install` in `web/` and `sdk/` to regenerate.

  Everything past the deps fetch is offline (the console's codegen + vite build; the runner's
  `bun build --target=bun` bundle). Both launchers exec `pkgs.bun` from the store — unlike the
  deb/rpm, which vendor a bun binary because apt/dnf have none.
- **Commit `flake.lock`:** it pins the input revisions (nixpkgs / crane / rust-overlay / bun2nix).
  It is generated on first eval and checked in.
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
