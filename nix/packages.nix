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
    license = with lib.licenses; [ mit asl20 ];
    platforms = [ "x86_64-linux" ]; # NVENC desktop host; matches the RPM's ExclusiveArch: x86_64
    maintainers = [ ];
  };
in
{
  punktfunk-host = craneLib.buildPackage (commonArgs // {
    pname = "punktfunk-host";
    cargoExtraArgs =
      "--locked -p punktfunk-host -p punktfunk-tray "
      + "--features punktfunk-host/nvenc,punktfunk-host/vulkan-encode";

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
      addDriverRunpath "$out/bin/punktfunk-host" "$out/bin/punktfunk-tray"
    '';

    meta = meta // {
      description = "Low-latency desktop/game streaming host (Moonlight-compatible + native punktfunk/1)";
      mainProgram = "punktfunk-host";
    };
  });

  punktfunk-client = craneLib.buildPackage (commonArgs // {
    pname = "punktfunk-client";
    # The session's default `ui` feature pulls skia-safe, whose build DOWNLOADS a prebuilt Skia
    # (rust-skia releases) — impossible in Nix's network-less build sandbox, and a from-source Skia
    # build needs the full gn/ninja/python toolchain + network-fetched third-party. The `ui` feature
    # is explicitly droppable (clients/session/Cargo.toml: "same streaming, stats on stdout only"),
    # so build the session without it. The GTK shell (punktfunk-client-linux) is skia-free and full.
    # Re-adding the Skia OSD under Nix is tracked in nix/README.md.
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
  });
}
