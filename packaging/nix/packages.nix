# punktfunk Nix packages — built with crane (github:ipetkov/crane).
#
# Two derivations, mirroring the RPM's `punktfunk` / `punktfunk-client` split (packaging/rpm/
# punktfunk.spec) so a headless streaming host never drags in the GTK client closure:
#
#   * punktfunk-host   — the streaming host + status tray (punktfunk-host, punktfunk-tray).
#                        Built with the same feature set CI ships: the direct-SDK NVENC path
#                        (`nvenc`) + the raw Vulkan-Video HEVC encoder (`vulkan-encode`). Both are
#                        AMD/Intel-safe (NVENC/CUDA entry points are dlopen'd at RUNTIME, never
#                        link-imported), so ONE binary runs on NVIDIA, AMD and Intel.
#   * punktfunk-client — the native Linux client: the GTK4/libadwaita shell (punktfunk-client) plus
#                        the Vulkan/ash session streamer it execs (punktfunk-session).
#
# Why crane and not rustPlatform.buildRustPackage:
#   * The lockfile carries `windows 0.62.2` (and friends) from BOTH crates.io AND a pinned
#     microsoft/windows-rs git rev — the Windows client uses the git one. importCargoLock keys its
#     vendor dir by `name-version`, so those same-name-same-version-different-source entries collide;
#     crane vendors per-source and handles it. (Those crates are `cfg(windows)`-gated and never
#     COMPILE on Linux — they only need to be vendored to satisfy the lock.)
#   * crane fetches git deps with `builtins.fetchGit` (the rev is a full sha ⇒ pure-eval-safe), so
#     no hand-maintained `outputHashes` for the windows-rs checkout.
#
# The whole workspace is built from real source (no crane dep-only "dummy" pre-build): pyrowave-sys
# runs CMake over its committed `vendor/pyrowave` tree in build.rs, which a dummy src would omit —
# so `cargoArtifacts = null` keeps deps + crate in one derivation.
{
  lib,
  craneLib,
  src,
  version,
  # native tooling
  pkg-config,
  cmake,
  nasm,
  perl,
  rustPlatform,
  addDriverRunpath,
  wrapGAppsHook4,
  # host + client shared libs
  ffmpeg,
  pipewire,
  libopus,
  wayland,
  libxkbcommon,
  libGL,
  vulkan-loader,
  # gbm lives in `libgbm` on recent nixpkgs, `mesa` on older ones.
  libgbm ? null,
  mesa,
  # client-only libs
  sdl3,
  gtk4,
  libadwaita,
  glib,
  librsvg,
  gsettings-desktop-schemas,
  adwaita-icon-theme,
  vulkan-headers,
  # web console (punktfunk-web): a bun-built Nitro SSR bundle, run on bun.
  bun,
  nodejs,
  makeWrapper,
  cacert,
  stdenvNoCC,
}:
let
  gbm = if libgbm != null then libgbm else mesa;

  # Build provenance stamped into the binary (build.rs reads PUNKTFUNK_BUILD_VERSION for
  # `--version` / mgmt /health), mirroring the deb/rpm `~ci`-style stamp.
  buildVersion = "${version}-nix";

  # `cargo build --release --locked` scoped to the crates each package ships. crane appends this to
  # both the vendor step and the compile.
  commonArgs = {
    inherit src version;
    strictDeps = true;

    # nixpkgs ships CMake ≥ 4, which errors on `cmake_minimum_required(VERSION <3.5)`. Several
    # vendored C libraries built through the `cmake` crate still declare a pre-3.5 minimum
    # (audiopus_sys' libopus; belt-and-braces for pyrowave-sys / aws-lc-sys). CMake reads this env
    # var as the floor policy version, letting those configure instead of aborting.
    CMAKE_POLICY_VERSION_MINIMUM = "3.5";

    # crane's default is a two-derivation split (buildDepsOnly → buildPackage); disable it, because
    # the deps-only pass builds a "dummy" source tree that drops pyrowave-sys's `vendor/pyrowave`
    # CMake inputs and would fail its build.rs. One derivation compiles everything from real src.
    cargoArtifacts = null;

    # The workspace has hardware-bound tests (uinput, a real GPU); packaging builds never run them,
    # exactly like the deb/rpm/PKGBUILD paths.
    doCheck = false;

    nativeBuildInputs = [
      pkg-config
      cmake # pyrowave-sys (C++/Vulkan), the vendored libopus (opus crate), aws-lc-sys (rustls)
      nasm # libopus SIMD + OpenH264 (openh264 `source` feature)
      perl # aws-lc-sys asm generation (rustls' aws-lc-rs crypto provider)
      rustPlatform.bindgenHook # LIBCLANG_PATH + clang args for ffmpeg-sys-next / pf-ffvk / pyrowave-sys bindgen
      addDriverRunpath # provides the `addDriverRunpath` shell fn used in postFixup
    ];
  };

  meta = {
    homepage = "https://git.unom.io/unom/punktfunk";
    license = with lib.licenses; [
      mit
      asl20
    ];
    platforms = [ "x86_64-linux" ]; # NVENC desktop host; matches the RPM's ExclusiveArch: x86_64
    maintainers = [ ];
  };

  # The Linux status tray, built in ISOLATION from the host — this is load-bearing, not tidiness.
  # Cargo unifies features across every package in a single `cargo build`, so co-building the tray
  # with the host would pull the host's `ashpd → zbus/tokio` onto the tray's SHARED `zbus`. The tray
  # deliberately runs zbus on `ksni`'s `async-io` executor with the `blocking` API and no tokio
  # runtime (see crates/punktfunk-tray/Cargo.toml), so a tokio-flavoured zbus panics at startup:
  # "there is no reactor running, must be called from the context of a Tokio 1.x runtime". A separate
  # invocation keeps the tray's zbus on async-io. (The host package copies this binary into its $out.)
  punktfunk-tray = craneLib.buildPackage (
    commonArgs
    // {
      pname = "punktfunk-tray";
      cargoExtraArgs = "--locked -p punktfunk-tray";
      PUNKTFUNK_BUILD_VERSION = buildVersion;
      # Pure-Rust leaf: ksni/zbus talk to the dbus socket, ureq+rustls(ring) + punktfunk-core `tls` —
      # nothing to link (no buildInputs) and no GPU driver runpath (it never dlopens libcuda/EGL/vulkan).
      meta = meta // {
        description = "punktfunk host status tray (Linux StatusNotifierItem)";
        mainProgram = "punktfunk-tray";
      };
    }
  );
in
{
  inherit punktfunk-tray;

  punktfunk-host = craneLib.buildPackage (
    commonArgs
    // {
      pname = "punktfunk-host";
      # HOST ONLY — the tray is a separate derivation (see the note above; co-building crashes it).
      cargoExtraArgs =
        "--locked -p punktfunk-host " + "--features punktfunk-host/nvenc,punktfunk-host/vulkan-encode";

      PUNKTFUNK_BUILD_VERSION = buildVersion;

      buildInputs = [
        ffmpeg # libavcodec/avformat/avutil (NVENC + VAAPI encode), via ffmpeg-next → ffmpeg-sys-next pkg-config
        pipewire # libpipewire-0.3 + libspa-0.2 (portal capture + the `pipewire` crate)
        libopus # audiopus_sys links system opus via pkg-config (else it vendors a static libopus that mis-links)
        wayland # libwayland-client (wlr / KWin fake_input backends)
        libxkbcommon # virtual-keyboard keymap build/validate
        libGL # src/linux/zerocopy/egl.rs `#[link(name = "GL")]`
        gbm # src/linux/zerocopy/egl.rs `#[link(name = "gbm")]`
        vulkan-loader # ash (Vulkan-Video encode + the dmabuf→VK bridge); loaded at runtime
      ];

      # KWin resolves a streaming client to a .desktop by matching /proc/<pid>/exe against `Exec`, so
      # the host's authorization .desktop must name the ACTUAL binary path (the store path here). The
      # tray/client .desktop Exec lines are rewritten for the same reason (menu launch / autostart).
      postInstall = ''
        # The status tray, built in its own derivation (see the punktfunk-tray note above) so its zbus
        # stays on async-io; ship it in the host's $out so the tray .desktop below resolves it.
        install -Dm0755 ${punktfunk-tray}/bin/punktfunk-tray "$out/bin/punktfunk-tray"

        # udev: /dev/uinput + /dev/uhid (virtual gamepads) + the vhci sysfs perms for the virtual Deck.
        install -Dm0644 scripts/60-punktfunk.rules "$out/lib/udev/rules.d/60-punktfunk.rules"

        # KWin Desktop-mode authorization (zkde_screencast + fake_input). Point Exec at the store binary.
        install -Dm0644 packaging/linux/io.unom.Punktfunk.Host.desktop \
          "$out/share/applications/io.unom.Punktfunk.Host.desktop"
        substituteInPlace "$out/share/applications/io.unom.Punktfunk.Host.desktop" \
          --replace-fail "/usr/bin/punktfunk-host" "$out/bin/punktfunk-host"

        # Status-tray autostart entry + its hicolor status icons.
        install -Dm0644 packaging/linux/io.unom.Punktfunk.Tray.desktop \
          "$out/etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop"
        substituteInPlace "$out/etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop" \
          --replace-fail "/usr/bin/punktfunk-tray" "$out/bin/punktfunk-tray"
        for sz in 22x22 48x48; do
          for png in packaging/linux/icons/hicolor/$sz/apps/*.png; do
            install -Dm0644 "$png" "$out/share/icons/hicolor/$sz/apps/$(basename "$png")"
          done
        done

        # Reference material: headless session helpers, example env files, the OpenAPI doc.
        install -Dm0755 scripts/headless/run-headless-kde.sh  "$out/share/punktfunk-host/headless/run-headless-kde.sh"
        install -Dm0755 scripts/headless/run-headless-sway.sh "$out/share/punktfunk-host/headless/run-headless-sway.sh"
        install -Dm0644 scripts/headless/kde-authorized       "$out/share/punktfunk-host/headless/kde-authorized"
        install -Dm0644 scripts/headless/punktfunk-sink.conf  "$out/share/punktfunk-host/headless/punktfunk-sink.conf"
        install -Dm0644 scripts/host.env.example              "$out/share/punktfunk-host/host.env.example"
        install -Dm0644 packaging/bazzite/host.env            "$out/share/punktfunk-host/host.env.bazzite"
        install -Dm0644 packaging/kde/host.env                "$out/share/punktfunk-host/host.env.kde"
        install -Dm0644 api/openapi.json                      "$out/share/punktfunk-host/openapi.json"
      '';

      # Run AFTER fixup (patchelf --shrink-rpath would otherwise drop /run/opengl-driver/lib, which is
      # empty at build time): append the driver runpath so the runtime dlopen of libcuda.so.1 /
      # libnvidia-encode.so.1 / libEGL.so.1 / the GPU's libvulkan ICD resolves from the running system.
      postFixup = ''
        # Only the host dlopens the GPU stack; the tray (its own derivation, copied in above) does not.
        addDriverRunpath "$out/bin/punktfunk-host"
      '';

      meta = meta // {
        description = "Low-latency desktop/game streaming host (Moonlight-compatible + native punktfunk/1)";
        mainProgram = "punktfunk-host";
      };
    }
  );

  punktfunk-client = craneLib.buildPackage (
    commonArgs
    // {
      pname = "punktfunk-client";
      # The session's default `ui` feature pulls skia-safe, whose build DOWNLOADS a prebuilt Skia
      # (rust-skia releases) — impossible in Nix's network-less build sandbox, and a from-source Skia
      # build needs the full gn/ninja/python toolchain + network-fetched third-party. The `ui` feature
      # is explicitly droppable (clients/session/Cargo.toml: "same streaming, stats on stdout only"),
      # so build the session without it. The GTK shell (punktfunk-client-linux) is skia-free and full.
      # Re-adding the Skia OSD under Nix is tracked in packaging/nix/README.md.
      cargoExtraArgs =
        "--locked -p punktfunk-client-linux -p punktfunk-client-session "
        + "--no-default-features --features punktfunk-client-session/pyrowave";

      # pf-ffvk runs bindgen over libavutil/hwcontext_vulkan.h, which `#include <vulkan/vulkan.h>`.
      # There is no /usr/include on NixOS, so hand it the Vulkan-Headers include dir explicitly
      # (build.rs turns this into a clang `-I`). libavutil's own include path comes from pkg-config.
      PF_FFVK_VULKAN_INCLUDE = "${vulkan-headers}/include";

      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ wrapGAppsHook4 ];

      buildInputs = [
        ffmpeg # FFmpeg decode (pf-client-core) + pf-ffvk links libavutil
        pipewire # PipeWire audio playback + mic capture
        libopus # audiopus_sys → system opus via pkg-config (Opus decode)
        sdl3 # window + gamepads (SDL3 HIDAPI: DualSense touchpad/motion/triggers)
        gtk4 # the GTK4 shell (clients/linux, relm4)
        libadwaita
        glib
        librsvg # gdk-pixbuf SVG loader for symbolic icons
        gsettings-desktop-schemas
        adwaita-icon-theme
        libxkbcommon
        libGL
        vulkan-loader # ash presenter + Vulkan-Video decode (loaded at runtime)
      ];

      # wrapGApp needs to run on the real ELF, and addDriverRunpath must run post-shrink — so disable
      # the hook's blanket auto-wrap and drive both by hand in postFixup (see the host note above).
      dontWrapGApps = true;

      postInstall = ''
        # hidraw access for the seated user's DualSense (SDL HIDAPI full-fidelity path).
        install -Dm0644 scripts/70-punktfunk-client.rules "$out/lib/udev/rules.d/70-punktfunk-client.rules"

        # Desktop launcher — point Exec at the store binary (wrapped below).
        install -Dm0644 packaging/linux/io.unom.Punktfunk.desktop \
          "$out/share/applications/io.unom.Punktfunk.desktop"
        substituteInPlace "$out/share/applications/io.unom.Punktfunk.desktop" \
          --replace-fail "Exec=punktfunk-client" "Exec=$out/bin/punktfunk-client"
      '';

      postFixup = ''
        addDriverRunpath "$out/bin/punktfunk-client" "$out/bin/punktfunk-session"
        # Only the GTK shell needs the GApps wrapper (GSETTINGS_SCHEMA_DIR, icon themes, typelibs);
        # the ash session binary is not a GTK app.
        wrapGApp "$out/bin/punktfunk-client"
      '';

      meta = meta // {
        description = "Native Linux punktfunk/1 streaming client (GTK4 shell + Vulkan session)";
        mainProgram = "punktfunk-client";
      };
    }
  );

  # --- management web console (punktfunk-web) ------------------------------------------------------
  # The browser console every client needs for SPAKE2 PIN pairing + host status: a TanStack Start /
  # React app that vite builds into a Nitro SSR bundle, run on `bun` (the Nitro `bun` preset + a
  # custom Bun.serve TLS entry — node can't run it; web/nitro-entry/bun-https.mjs). This mirrors the
  # Debian punktfunk-web .deb (packaging/debian/build-web-deb.sh) and the RPM's `--with web`
  # subpackage, which the host package Recommends so a default install pulls the console too.
  #
  # Unlike apt/dnf — which have no bun in their repos and so VENDOR a bun binary into the package —
  # Nix has `pkgs.bun`, so the launcher just execs it from the store (no vendored runtime). The
  # systemd `--user` units + firewall wiring live in the NixOS module, pointed at this store path.
  punktfunk-web =
    let
      # Offline node_modules for the console. `bun install` needs the network AND the @unom npm
      # registry (web/.npmrc → https://git.unom.io/api/packages/unom/npm/, read-public: the same
      # anonymous pull CI's rpm/deb builds do), so it lives in a fixed-output derivation — FODs get
      # network, and `outputHash` pins the result. `--ignore-scripts` skips the install-time
      # `prepare` codegen (it wants ../api/openapi.json, outside this web-only src scope); the build
      # derivation below runs codegen itself where the whole tree is present.
      #
      # ⚠ When web/bun.lock changes, this hash must be refreshed: set `outputHash = lib.fakeHash`,
      # rebuild, and copy the sha256 Nix reports back here (see packaging/nix/README.md).
      webDeps = stdenvNoCC.mkDerivation {
        pname = "punktfunk-web-deps";
        inherit version;
        src = src + "/web";
        nativeBuildInputs = [
          bun
          cacert
        ];
        dontConfigure = true;
        buildPhase = ''
          runHook preBuild
          export HOME=$TMPDIR
          export BUN_INSTALL_CACHE_DIR=$TMPDIR/bun-cache
          export SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt
          export NODE_EXTRA_CA_CERTS=$SSL_CERT_FILE
          # copyfile backend ⇒ node_modules is fully materialised (no links into the ephemeral
          # cache), so the tree survives the copy into the content-addressed $out.
          bun install --frozen-lockfile --ignore-scripts --no-progress --backend=copyfile
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          mkdir -p $out
          cp -R node_modules $out/node_modules
          runHook postInstall
        '';
        dontFixup = true;
        outputHashMode = "recursive";
        outputHashAlgo = "sha256";
        outputHash = "sha256-5oVZv65SMvq9i2REzHE8Pyn6qUZaV2FnPQdaouwcwoU="; # web/bun.lock deps (refresh on lockfile change; see README).
      };
    in
    stdenvNoCC.mkDerivation {
      pname = "punktfunk-web";
      inherit src version;
      # nodejs: the JS build tools' `.bin` shims are `#!/usr/bin/env node`; patchShebangs (below)
      # repoints them at this node so they run in the sandbox. bun is still the RUNTIME (the launcher
      # execs it); node is build-time only, for orval/paraglide/vite.
      nativeBuildInputs = [
        bun
        nodejs
        makeWrapper
      ];

      # No cross-derivation dep cache: codegen + the vite build are fully offline (every input is in
      # the vendored node_modules, the checked-in api/openapi.json, and web/project.inlang).
      #
      # ⚠ "Offline" is load-bearing and NOT self-enforcing. inlang resolves the plugins in
      # web/project.inlang/settings.json `modules`, and a failed import is only a WARNING there:
      # paraglide then prints "Successfully compiled", exits 0, and emits ZERO messages, so the
      # console builds fine and dies at SSR time with every `m.foo()` undefined. That is exactly
      # what a CDN URL in `modules` did in this network-less sandbox. The plugin is now a normal
      # devDependency referenced by path, and `bun run codegen` ends in tools/check-i18n.mjs, which
      # fails the build on a remote module or a short message count. Keep both properties.
      buildPhase = ''
        runHook preBuild
        export HOME=$TMPDIR
        cp -R ${webDeps}/node_modules web/node_modules
        chmod -R u+w web/node_modules
        # The JS CLIs (orval, paraglide-js, vite, …) ship a `#!/usr/bin/env node` shebang, and the
        # build sandbox has no /usr/bin/env — rewrite them to the store `node` before running any
        # script (else `bun run codegen` dies with "bad interpreter: /usr/bin/env"). Patch the WHOLE
        # node_modules, not just .bin: bun's .bin entries are symlinks (skipped by patchShebangs'
        # `-type f`); the real shebang lives in each package's `dist/bin/*.js` that they point to.
        patchShebangs web/node_modules
        cd web
        # `codegen` = orval (a typed React-Query client from ../api/openapi.json) + paraglide-js i18n
        # compile; both write into src/ and are prerequisites of the build (normally the install-time
        # `prepare` hook, which was skipped in the deps FOD).
        bun run codegen
        # `build` = vite build ⇒ the Nitro `bun`-preset SSR bundle in .output (our Bun.serve TLS entry).
        bun run build
        # Guard: assert we produced the bun bundle, not a node one (same check the deb/rpm builders do).
        grep -q 'Bun\.serve' .output/server/index.mjs \
          || { echo "ERROR: web/.output is not a bun bundle (wrong nitro preset)" >&2; exit 1; }
        cd ..
        runHook postBuild
      '';

      installPhase = ''
        runHook preInstall
        # The SSR bundle + its static assets, plus the first-run helper and env sample.
        mkdir -p $out/share/punktfunk-web/.output
        cp -R web/.output/server $out/share/punktfunk-web/.output/server
        cp -R web/.output/public $out/share/punktfunk-web/.output/public
        install -Dm0755 scripts/web-init.sh $out/share/punktfunk-web/web-init.sh
        install -Dm0644 web/web.env.example $out/share/punktfunk-web/web.env.example

        # PATH-stable launcher: run the SSR bundle on bun from the store (mirrors the deb/rpm
        # /usr/bin/punktfunk-web-server, minus the vendored-bun indirection).
        makeWrapper ${bun}/bin/bun $out/bin/punktfunk-web-server \
          --add-flags "$out/share/punktfunk-web/.output/server/index.mjs"
        runHook postInstall
      '';

      dontFixup = true;

      meta = meta // {
        description = "punktfunk management web console (Nitro SSR on bun + React)";
        mainProgram = "punktfunk-web-server";
      };
    };

  # --- plugin/script runner (punktfunk-scripting) --------------------------------------------------
  # The host's automation runner: the `@punktfunk/host` SDK's `punktfunk-scripting` CLI (built on
  # Effect), which discovers ~/.config/punktfunk/{scripts,plugins} and supervises each unit as an
  # Effect fiber. It runs on `bun` (it import()s the operator's `.ts` plugin files, which only bun
  # can do). Mirrors the Debian punktfunk-scripting .deb / the RPM's `--with scripting` subpackage,
  # which the host package Recommends — the NixOS module wires the opt-in systemd --user unit.
  #
  # Unlike the deb/rpm we don't `bun build` into a bundle + vendor bun; we still bundle (one
  # self-contained JS, effect inlined) but the launcher execs `pkgs.bun` from the store.
  punktfunk-scripting =
    let
      # Offline node_modules for the SDK build — same fixed-output pattern as punktfunk-web's webDeps
      # (`bun install` needs the network). ⚠ Refresh `outputHash` when sdk/bun.lock changes (set
      # lib.fakeHash, rebuild, copy the printed sha256 — see packaging/nix/README.md).
      sdkDeps = stdenvNoCC.mkDerivation {
        pname = "punktfunk-scripting-deps";
        inherit version;
        src = src + "/sdk";
        nativeBuildInputs = [
          bun
          cacert
        ];
        dontConfigure = true;
        buildPhase = ''
          runHook preBuild
          export HOME=$TMPDIR
          export BUN_INSTALL_CACHE_DIR=$TMPDIR/bun-cache
          export SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt
          export NODE_EXTRA_CA_CERTS=$SSL_CERT_FILE
          bun install --frozen-lockfile --ignore-scripts --no-progress --backend=copyfile
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          mkdir -p $out
          cp -R node_modules $out/node_modules
          runHook postInstall
        '';
        dontFixup = true;
        outputHashMode = "recursive";
        outputHashAlgo = "sha256";
        outputHash = "sha256-+KCKCA0q0bwTxr7bsA3X4DbT/8nUjJA/JIoJU6BfiZw="; # sdk/bun.lock deps (refresh on lockfile change; see README).
      };
    in
    stdenvNoCC.mkDerivation {
      pname = "punktfunk-scripting";
      inherit src version;
      nativeBuildInputs = [
        bun
        makeWrapper
      ];

      # `bun build --target=bun` bundles the runner CLI to ONE self-contained JS: effect + the SDK
      # are inlined, and the runner's dynamic `import()` of the operator's plugin files is left as a
      # runtime import (bun keeps unresolvable dynamic specifiers external). Fully offline.
      buildPhase = ''
        runHook preBuild
        export HOME=$TMPDIR
        cp -R ${sdkDeps}/node_modules sdk/node_modules
        chmod -R u+w sdk/node_modules
        ( cd sdk && bun build src/runner-cli.ts --target=bun --outfile=$TMPDIR/runner-cli.js )
        grep -q 'attempt=' $TMPDIR/runner-cli.js \
          || { echo "ERROR: runner bundle missing the dynamic plugin import — wrong build" >&2; exit 1; }
        runHook postBuild
      '';

      installPhase = ''
        runHook preInstall
        install -Dm0644 $TMPDIR/runner-cli.js $out/share/punktfunk-scripting/runner-cli.js
        # Launcher: run the bundle on bun from the store (mirrors the deb/rpm /usr/bin/punktfunk-scripting).
        makeWrapper ${bun}/bin/bun $out/bin/punktfunk-scripting \
          --add-flags "$out/share/punktfunk-scripting/runner-cli.js"
        runHook postInstall
      '';

      dontFixup = true;

      meta = meta // {
        description = "punktfunk plugin/script runner (Effect SDK on bun)";
        mainProgram = "punktfunk-scripting";
      };
    };
}
