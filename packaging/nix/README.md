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
| `checks.x86_64-linux.nixos-module` | evaluates the NixOS module against real nixpkgs and asserts on the rendered systemd units |
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
            desktopSession = true;      # a machine you log into — restart the host with the desktop
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

Then, in your graphical session (the console follows with `punktfunk-web`; the plugin runner is
already started for you — see `scripting.autoStart` below):

```sh
systemctl --user enable --now punktfunk-host punktfunk-web
```

### Options

`services.punktfunk.host`:

| Option | Default | Meaning |
| --- | --- | --- |
| `enable` | `false` | Install the host + wire udev/sysctl/kernel-modules/firewall and the user service. |
| `gamestream` | `true` | `serve --gamestream` (Moonlight-compatible). `false` = native-only, more secure. |
| `autoStart` | `false` | Add the user service to `default.target` (appliance mode — pair with lingering). |
| `desktopSession` | `false` | Bind the host to `graphical-session.target` — **turn this on for a machine somebody logs into** (see below). |
| `users` | `[ ]` | Users added to the `input` **and `punktfunk`** groups (virtual gamepads; the second covers the usbip/vhci nodes the virtual Steam Deck pad attaches through — it can emulate arbitrary USB hardware, so list only users you'd trust with that). |
| `settings` | `{ }` | `host.env` key/values (see `${package}/share/punktfunk-host/host.env.example`). |
| `environmentFile` | `null` | Extra `EnvironmentFile` for secrets (e.g. `PUNKTFUNK_MGMT_TOKEN`); loaded optionally. |
| `openFirewall` | `false` | Open the inbound ports (see below). |
| `gamescopeHdr` | `true` | Put `punktfunk-gamescope` (gamescope + our `pipewire-hdr` patches) on the service PATH, so a 10-bit client can stream true HDR10 off a gamescope output. Costs a gamescope build from source — set `false` to skip it and stay SDR on that backend. |
| `gamescopePackage` | flake's | The patched gamescope used when `gamescopeHdr = true`. |
| `package` | flake's | Override the package. |

**`desktopSession` — set it on a desktop, leave it off on an appliance.** On a machine somebody logs
into, a compositor restart (a crash, a logout/login, "restart the shell") otherwise leaves the host
running while it holds a Wayland socket and a portal D-Bus connection that both died with the old
compositor. It cannot recover either in-process, and the failure is *silent*: the host still
listens, still answers, and every session it then serves fails at capture. `desktopSession = true`
adds `PartOf=`/`WantedBy=graphical-session.target` (in addition to `default.target`), so the host
restarts with the session. Leave it `false` for an appliance — a pinned `PUNKTFUNK_COMPOSITOR`, a
headless KWin or a gamescope box — which may never reach that target and would be left permanently
stopped. sway/Hyprland and anything else not under systemd session management never reach it
either; there, start the host from the compositor's config after `systemctl --user
import-environment`.

**Portals.** The host reaches the desktop through `xdg-desktop-portal` on several backends (Mutter's
ScreenCast/RemoteDesktop, and the libei input path), so a hand-assembled machine wants
`xdg.portal.enable = true` plus the backend for its compositor
(`xdg-desktop-portal-kde` / `-gnome` / `-hyprland` / `-wlr`). The KDE and GNOME desktop-manager
modules already do this. The module emits a warning if the host is enabled and portals are not —
the KWin backend's own virtual output uses the privileged `zkde_screencast` protocol and needs no
portal, so KDE-only setups are unaffected in practice.

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

`services.punktfunk.scripting` (the plugin/script runner — installed **and started** with the host,
matching the deb/rpm, which `systemctl --global enable` it):

| Option | Default | Meaning |
| --- | --- | --- |
| `enable` | `host.enable` | Install the runner + define its `systemd --user` unit `punktfunk-scripting`. |
| `autoStart` | `scripting.enable` | Add the unit to `default.target`. **On by default** — the game-library scanners are plugins, so a host without the runner has an empty library. |
| `package` | flake's | Override the package. |

The runner discovers loose scripts under `~/.config/punktfunk/scripts` and installed
`punktfunk-plugin-*` packages under `~/.config/punktfunk/plugins`, and supervises each as an Effect
fiber (SIGTERM shuts the tree down structurally so plugin finalizers run). A plugin auto-wires to
the host's mgmt token + identity cert.

It used to ship un-started here, on the reasoning that the runner is inert until you add
automation. That stopped being true when the library scanners became plugins — a host with the
runner off comes up with an empty library and no obvious reason why — so it now runs by default,
as it already did on every other channel. Opt out with `scripting.autoStart = false`, or per user
`systemctl --user mask punktfunk-scripting` (`mask`, not `disable`).

The runner is sandboxed exactly as the deb/rpm unit is (`NoNewPrivileges`, `ProtectSystem=strict`,
`ReadWritePaths=%h /tmp`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`) — with `PrivateTmp`
deliberately **off**, because plugins integrate with things that talk over `/tmp`. `ProtectSystem`
on a *user* unit needs unprivileged user namespaces; drop it with
`systemctl --user edit punktfunk-scripting` on a kernel that restricts them.

### What the host module configures for you

Everything the RPM's `%install` + `%post` do, declaratively:

- **systemd `--user` service** `punktfunk-host` → `serve [--gamestream]`, `EnvironmentFile` from
  `settings` (+ optional secret file), `Restart=on-failure`, and — with `desktopSession` —
  `PartOf=graphical-session.target`.
- **udev rules** (`60-punktfunk.rules`): `/dev/uinput` + `/dev/uhid` group access and the vhci
  sysfs perms for the virtual Steam Deck.
- **kernel modules**: `uinput`, `uhid`, `vhci-hcd` (usbip transport so Steam Input adopts the
  virtual Deck).
- **sysctl**: `net.core.{r,w}mem_max = 32 MB` (high-bitrate UDP headroom; `mkDefault`).
- **`input` and `punktfunk` groups**, declared and joined for `users`. Both are required: the udev
  rule `chgrp punktfunk`s the vhci nodes and fails outright if nothing ever created that group.
- **A `security.wrappers` entry for `punktfunk-encode-worker`** carrying `cap_sys_nice=ep`, with
  `PUNKTFUNK_ENCODE_WORKER` pointed at it. A file capability cannot live on a read-only store path,
  so a wrapper is the only mechanism NixOS has. The capability is deliberately **not** on the host
  itself — see the caveat below.
- **`hardware.graphics.enable = true`** (`mkDefault`) so `/run/opengl-driver/lib` has the driver
  libs the binaries `dlopen`.
- **firewall** (when `openFirewall`): native UDP 9777/5353 + TCP 47990; with `gamestream` also TCP
  47984/47989/48010 + UDP 47998/47999/48000; with the console, TCP 47992 + 47993. The media data
  plane is an ephemeral, hole-punched UDP port — nothing fixed to open.
- **tray autostart** entry (`--autostart`; self-gates to users who actually run a host).
- **A warning** if `xdg.portal.enable` is off (see the portal note above).

> **Why the capability is on the worker and not the host.** KWin only advertises its restricted
> protocols (`zkde_screencast_unstable_v1` for the virtual output, `org_kde_kwin_fake_input` for
> input) to a client it can *identify*, by resolving that client's `/proc/<pid>/exe` and matching an
> installed `.desktop`'s `Exec=`. The kernel refuses that readlink to any reader whose effective set
> is not a superset of the target's permitted set, and KWin holds no capabilities. A NixOS wrapper
> does not dodge this — it raises the capability into the ambient set before exec'ing, which lands
> it in the permitted set and fails the readlink identically. Giving the host `cap_sys_nice` broke
> desktop streaming on every KDE box in 0.26.0-1. The encode worker is a separate binary that
> nothing ever has to identify, so the grant is safe there.

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
```

Leave `desktopSession` off here — an appliance starts its own compositor and may never reach
`graphical-session.target`, which would leave the host permanently stopped. `gamescopeHdr` (on by
default) already puts the patched `punktfunk-gamescope` on the service PATH, so the gamescope
backend needs no PATH surgery; extend it only for a helper the module doesn't know about:

```nix
# systemd.user.services.punktfunk-host.path = [ pkgs.some-helper ];
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

The shell exports an
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
  broke the build. `bun2nix` is a devDependency of both packages and regenerates `bun.nix` on every
  `bun install` (web's `postinstall`; the SDK's `prepare`, since sdk/ is the *published*
  `@punktfunk/host` package and a `postinstall` would then fire on consumers' installs).
  The `@unom` scope needs no special handling: `web/bun.lock` records those tarballs' full
  `https://git.unom.io/api/packages/unom/npm/…` URLs and the registry is read-public (the same
  anonymous pull CI's rpm/deb builds do).

  > ⚠⚠ **That devDependency hook is a convenience, NOT the guarantee — `bun.nix` still drifts.**
  > It fires only on a local `bun install` that runs lifecycle scripts. It does *not* fire under
  > `bun install --ignore-scripts`, which is what every bun install in CI uses; and it cannot fire
  > on a **merge or rebase**, where git carries someone else's `bun.lock` change past a `bun.nix`
  > generated before it and reports no conflict. That is how `web/bun.nix` shipped on main holding
  > `brace-expansion@5.0.7` while `web/bun.lock` said `5.0.8` — for **553 commits** (2026-07-27 →
  > 2026-08-05), with `nix build .#punktfunk-web` broken the whole time, until an unrelated
  > advisory bump happened to rerun a real `bun install` and closed it by accident.
  >
  > The enforcement point is **`scripts/ci/check-bun-nix.sh`** (the `bun-nix` job in `ci.yml`,
  > unfiltered so it sees the innocuous-looking commits drift arrives through). It regenerates each
  > `bun.nix` from its committed `bun.lock` and diffs. Fix any report with:
  >
  >     scripts/ci/check-bun-nix.sh --fix
  >
  > Never regenerate with a bare `bunx bun2nix`: **`bun.nix` has no schema stability across bun2nix
  > versions**, and an unpinned `bunx` uses whatever is newest. The flake input
  > (`github:nix-community/bun2nix?ref=2.1.2`) and the npm devDependency in `web/package.json` +
  > `sdk/package.json` must name the *same exact version* — the script checks that too, and always
  > generates with the pinned one. Move all three together, then rerun it with `--fix`.

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

- **⚠ `nix flake check` does NOT check the NixOS module — that is why `module-check.nix` exists.**
  For `nixosModules`, nix forces the value and asserts it is a lambda taking an open attribute set,
  and stops there (its source still carries `// FIXME: if we have a 'nixpkgs' input, use it to check
  the module.`). Measured: a module setting a nonexistent *option*, referencing a nonexistent
  `pkgs` attribute **and** calling a nonexistent `lib` function passes clean, printing
  `checking NixOS module 'nixosModules.default'... all checks passed!`. So the reassuring line means
  nothing. `checks.<system>.nixos-module` (`packaging/nix/module-check.nix`) closes it: it evaluates
  the module against real nixpkgs in four scenarios and asserts on the rendered systemd units.
  Two rules if you edit it — **keep every assertion pure Nix** (instantiating the derivation is what
  runs them, which is what lets the cheap `--no-build` CI leg cover it; a shell script in the
  `runCommand` body would only run under a full `nix flake check`, i.e. an hour of Rust), and
  **assert list-valued unit fields on the evaluated lists**, not the rendered text — systemd renders
  `After=` as one space-separated line, so an `hasInfix` on it silently depends on ordering.

## Verified

The packages build, install, and run on real Nix hardware (NixOS-equivalent: CachyOS + Nix,
RTX 5070 Ti, driver 610). `punktfunk-host --version` and `punktfunk-session` run; the driver
RUNPATH (`/run/opengl-driver/lib`) and the GTK GApps wrapper (GSettings schemas + pixbuf loaders)
are present. Fixes discovered during that bring-up: `CMAKE_POLICY_VERSION_MINIMUM=3.5` (CMake ≥ 4),
system `libopus` (audiopus_sys), and the session Skia note above.

In CI (`.gitea/workflows/nix.yml`): `nix flake check --no-build` evaluates every output *including*
the module check above, and `punktfunk-web` + `punktfunk-scripting` are built for real. The Rust
packages and `punktfunk-gamescope` are `workflow_dispatch` opt-ins (`build-rust`,
`build-gamescope`) — run the latter after a `flake.lock` bump, since it patches whatever gamescope
the pinned nixpkgs carries.
