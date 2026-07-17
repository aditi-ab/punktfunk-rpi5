{
  description = "punktfunk — low-latency desktop/game streaming host + native Linux client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
    }:
    let
      # Linux/x86_64 only — the host encodes with desktop NVENC and CI publishes no aarch64 leg
      # (mirrors the RPM's `ExclusiveArch: x86_64`). Add a system here once there's an arm64 build.
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);

      # The workspace version is the single source of truth (crates/*/Cargo.toml inherit it).
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ (import rust-overlay) ];
      };

      # Pin cargo/rustc EXACTLY to rust-toolchain.toml (channel 1.96.0 + rustfmt/clippy) so a Nix
      # build, a dev shell and CI all use the identical toolchain — the repo is deliberate about
      # this (see rust-toolchain.toml's header on rustfmt format-drift).
      toolchainFor = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

      craneLibFor = pkgs: (crane.mkLib pkgs).overrideToolchain toolchainFor;

      packagesFor = system:
        let
          pkgs = pkgsFor system;
        in
        pkgs.callPackage ./packaging/nix/packages.nix {
          craneLib = craneLibFor pkgs;
          src = self;
          inherit version;
        };
    in
    {
      packages = forAllSystems (system:
        let pf = packagesFor system;
        in {
          inherit (pf) punktfunk-host punktfunk-client punktfunk-web punktfunk-scripting punktfunk-tray;
          default = pf.punktfunk-host;
        });

      # `nix run .#punktfunk-host -- serve` / `nix run .#punktfunk-client`.
      apps = forAllSystems (system:
        let pf = packagesFor system;
        in {
          punktfunk-host = {
            type = "app";
            program = "${pf.punktfunk-host}/bin/punktfunk-host";
          };
          punktfunk-client = {
            type = "app";
            program = "${pf.punktfunk-client}/bin/punktfunk-client";
          };
          # `nix run .#punktfunk-web` — the console (auto-wire the mgmt token / cert via env or the
          # NixOS module; see packaging/nix/README.md).
          punktfunk-web = {
            type = "app";
            program = "${pf.punktfunk-web}/bin/punktfunk-web-server";
          };
          # `nix run .#punktfunk-scripting -- --list` — the plugin/script runner.
          punktfunk-scripting = {
            type = "app";
            program = "${pf.punktfunk-scripting}/bin/punktfunk-scripting";
          };
          default = self.apps.${system}.punktfunk-host;
        });

      # `nix flake check` builds every package (web included — needs its deps hash filled in, see
      # packaging/nix/README.md).
      checks = forAllSystems (system:
        let pf = packagesFor system;
        in {
          inherit (pf) punktfunk-host punktfunk-client punktfunk-web punktfunk-scripting;
        });

      # `nix develop` — the pinned toolchain plus every system lib the workspace links, wired so
      # `cargo build` (all crates, host + client) works out of the box.
      devShells = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          toolchain = toolchainFor pkgs;
          gbm = pkgs.libgbm or pkgs.mesa;
        in
        {
          default = pkgs.mkShell {
            strictDeps = false;
            nativeBuildInputs = [
              toolchain
              pkgs.rust-analyzer
              pkgs.pkg-config
              pkgs.cmake
              pkgs.nasm
              pkgs.perl
              pkgs.rustPlatform.bindgenHook
            ];
            buildInputs = [
              # host
              pkgs.ffmpeg
              pkgs.pipewire
              pkgs.libopus
              pkgs.wayland
              pkgs.libxkbcommon
              pkgs.libGL
              gbm
              pkgs.vulkan-loader
              # client
              pkgs.sdl3
              pkgs.gtk4
              pkgs.libadwaita
              pkgs.glib
              pkgs.librsvg
              pkgs.gsettings-desktop-schemas
              pkgs.adwaita-icon-theme
              pkgs.vulkan-headers
            ];
            # pf-ffvk bindgen (no /usr/include on NixOS); GPU driver libs at runtime for `cargo run`.
            PF_FFVK_VULKAN_INCLUDE = "${pkgs.vulkan-headers}/include";
            # CMake ≥ 4 rejects the pre-3.5 minimums some vendored C libs (libopus) still declare.
            CMAKE_POLICY_VERSION_MINIMUM = "3.5";
            LD_LIBRARY_PATH = "/run/opengl-driver/lib:${pkgs.lib.makeLibraryPath [ pkgs.vulkan-loader pkgs.libGL gbm ]}";
          };
        });

      formatter = forAllSystems (system: (pkgsFor system).nixfmt-rfc-style);

      # NixOS integration — see packaging/nix/nixos-module.nix and packaging/nix/README.md.
      #   imports = [ punktfunk.nixosModules.default ];
      #   services.punktfunk.host.enable = true;
      nixosModules.default = import ./packaging/nix/nixos-module.nix self;
      nixosModules.punktfunk = self.nixosModules.default;
    };
}
