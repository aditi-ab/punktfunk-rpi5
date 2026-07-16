# NixOS integration for punktfunk — the declarative equivalent of everything the RPM/deb do in
# their %install + %post (packaging/rpm/punktfunk.spec, packaging/debian/build-deb.sh):
# the systemd *user* service, the uinput/uhid/vhci udev rules, the vhci-hcd autoload, the 32 MB
# UDP socket-buffer sysctls, the firewall openers, and the `input`-group membership for virtual
# gamepads.
#
# Usage (flake):
#   { inputs.punktfunk.url = "git+https://git.unom.io/unom/punktfunk";
#     outputs = { punktfunk, nixpkgs, ... }: {
#       nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
#         modules = [ punktfunk.nixosModules.default
#                     { services.punktfunk.host.enable = true;
#                       services.punktfunk.host.users = [ "alice" ]; } ];
#       };
#     };
#   }
#
# The host is fundamentally a per-user, in-graphical-session service (it drives the live
# compositor, PipeWire and the desktop portals), so it ships as a `systemd.user` unit. Enable it
# for a session with `systemctl --user enable --now punktfunk-host` (or set `autoStart = true` for a
# headless appliance with `users.users.<u>.linger = true`).
self:
{ config, lib, pkgs, ... }:
let
  inherit (lib)
    mkEnableOption mkOption mkIf mkMerge mkDefault types
    optional optionals optionalString literalExpression
    concatStringsSep mapAttrsToList genAttrs;

  cfg = config.services.punktfunk;
  system = pkgs.stdenv.hostPlatform.system;

  # host.env rendering: booleans → 1/0 (what PUNKTFUNK_* knobs expect), everything else verbatim.
  renderVal = v:
    if lib.isBool v then (if v then "1" else "0")
    else toString v;
  renderEnv = attrs:
    concatStringsSep "\n" (mapAttrsToList (k: v: "${k}=${renderVal v}") attrs) + "\n";

  hostSettingsFile = pkgs.writeText "punktfunk-host.env" (renderEnv cfg.host.settings);

  # Native punktfunk/1 ports (control plane + discovery + mgmt API). The media data plane is an
  # ephemeral per-session UDP port the host hole-punches, so nothing fixed to open (see
  # packaging/linux/punktfunk.ufw).
  nativeTCP = [ 47990 ]; # mgmt/library REST API (HTTPS + mTLS)
  nativeUDP = [ 9777 5353 ]; # QUIC control plane + mDNS
  # GameStream/Moonlight-compat fixed ports (opt-in with `host.gamestream`).
  gamestreamTCP = [ 47984 47989 48010 ];
  gamestreamUDP = [ 47998 47999 48000 ];
in
{
  options.services.punktfunk = {
    host = {
      enable = mkEnableOption "the punktfunk streaming host (systemd --user service + system wiring)";

      package = mkOption {
        type = types.package;
        default = self.packages.${system}.punktfunk-host;
        defaultText = literalExpression "punktfunk.packages.\${system}.punktfunk-host";
        description = "The punktfunk-host package (bundles punktfunk-host + punktfunk-tray).";
      };

      gamestream = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Advertise the GameStream/Moonlight-compatible planes (`serve --gamestream`) so a stock
          Moonlight client can pair. Set to `false` for a native-only, more secure host (no
          plain-HTTP pairing / legacy GCM path) and drop the GameStream firewall ports.
        '';
      };

      autoStart = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Start the host automatically in every user's graphical session (adds it to the user
          `default.target`). For a login-less appliance, also enable lingering for the host user
          (`users.users.<name>.linger = true`) so the user service comes up at boot.
        '';
      };

      users = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "alice" ];
        description = ''
          Users to add to the `input` group — required for the virtual gamepads the host creates
          (`/dev/uinput`, `/dev/uhid`, and the usbip/vhci virtual Steam Deck). The host runs as
          these users' `systemd --user` service.
        '';
      };

      settings = mkOption {
        type = types.attrsOf (types.oneOf [ types.str types.int types.bool ]);
        default = { };
        example = literalExpression ''
          {
            PUNKTFUNK_VIDEO_SOURCE = "virtual";
            PUNKTFUNK_COMPOSITOR = "kwin";
            PUNKTFUNK_444 = true;
            RUST_LOG = "info";
          }
        '';
        description = ''
          `host.env` key/value pairs passed to the service via `EnvironmentFile`. See
          `''${package}/share/punktfunk-host/host.env.example` for the full surface. Booleans render
          as `1`/`0`. Leave empty to rely on the host's per-connect auto-detection of the
          compositor + input backend. Do NOT put secrets here (world-readable in the store) — use
          `environmentFile` instead.
        '';
      };

      environmentFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        example = "/run/secrets/punktfunk-host.env";
        description = ''
          Extra `EnvironmentFile` layered AFTER `settings` (its values win). For secrets such as
          `PUNKTFUNK_MGMT_TOKEN`. Loaded optionally (a missing file does not fail the unit).
        '';
      };

      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Open the host's inbound ports. Native punktfunk/1 always: UDP 9777 (QUIC) + 5353 (mDNS),
          TCP 47990 (mgmt API). With `gamestream = true` also TCP 47984/47989/48010 and UDP
          47998/47999/48000. The ephemeral media UDP port is hole-punched, so a default-deny
          firewall still streams (it just adds ~2.5 s at session start).
        '';
      };
    };

    client = {
      enable = mkEnableOption "the native punktfunk Linux client";

      package = mkOption {
        type = types.package;
        default = self.packages.${system}.punktfunk-client;
        defaultText = literalExpression "punktfunk.packages.\${system}.punktfunk-client";
        description = "The punktfunk-client package (bundles punktfunk-client + punktfunk-session).";
      };

      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = "Open UDP 5353 (mDNS) so the client can auto-discover hosts on the LAN.";
      };
    };
  };

  config = mkMerge [
    # --- shared: whenever either half is enabled -----------------------------------------------
    (mkIf (cfg.host.enable || cfg.client.enable) {
      assertions = [{
        assertion = system == "x86_64-linux";
        message = "services.punktfunk is x86_64-linux only (desktop NVENC host; no aarch64 build).";
      }];
      # The GPU driver libs the binaries dlopen at runtime (libcuda / libnvidia-encode / libEGL /
      # the Vulkan ICD) live under /run/opengl-driver/lib — provided by hardware.graphics.
      hardware.graphics.enable = mkDefault true;
      # 32 MB UDP socket buffers — without this the kernel clamps the host's SO_SNDBUF / client's
      # SO_RCVBUF and high-bitrate frames overflow (measured: 4 MB cap = 31.6 % loss at 2 Gbps).
      boot.kernel.sysctl = {
        "net.core.wmem_max" = mkDefault 33554432;
        "net.core.rmem_max" = mkDefault 33554432;
      };
    })

    # --- host ----------------------------------------------------------------------------------
    (mkIf cfg.host.enable {
      environment.systemPackages = [ cfg.host.package ];
      # 60-punktfunk.rules: /dev/uinput + /dev/uhid group access + the vhci sysfs perms.
      services.udev.packages = [ cfg.host.package ];
      # The vhci attach/detach rule shells out to chgrp/chmod (coreutils) — put them on udev's PATH.
      services.udev.path = [ pkgs.coreutils ];
      # uinput/uhid: the virtual X360 + DualSense nodes. vhci-hcd: the usbip transport that makes
      # the virtual Steam Deck a real USB device (Steam Input only adopts USB pads).
      boot.kernelModules = [ "uinput" "uhid" "vhci-hcd" ];

      # `input` group membership for the virtual-gamepad nodes (mirrors the RPM's usermod hint).
      users.groups.input = { };
      users.users = genAttrs cfg.host.users (_: { extraGroups = [ "input" ]; });

      # Status-tray autostart entry (self-gating: `--autostart` exits unless this user runs a host).
      environment.etc."xdg/autostart/io.unom.Punktfunk.Tray.desktop".source =
        "${cfg.host.package}/etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop";

      networking.firewall = mkIf cfg.host.openFirewall {
        allowedTCPPorts = nativeTCP ++ optionals cfg.host.gamestream gamestreamTCP;
        allowedUDPPorts = nativeUDP ++ optionals cfg.host.gamestream gamestreamUDP;
      };

      systemd.user.services.punktfunk-host = {
        description = "punktfunk GameStream + punktfunk/1 streaming host";
        documentation = [ "https://git.unom.io/unom/punktfunk" ];
        # Soft ordering: the host listens immediately and only touches the compositor per session.
        after = [ "pipewire.service" ];
        wants = [ "pipewire.service" ];
        wantedBy = optional cfg.host.autoStart "default.target";
        # The host may exec external helpers (pw-dump, sh, and — for the gamescope/kwin backends —
        # the compositor). Extend this in your config for a headless gamescope/KWin appliance.
        path = [ pkgs.bash pkgs.coreutils pkgs.pipewire ];
        serviceConfig = {
          ExecStart = "${cfg.host.package}/bin/punktfunk-host serve"
            + optionalString cfg.host.gamestream " --gamestream";
          Restart = "on-failure";
          RestartSec = 2;
          EnvironmentFile =
            (optional (cfg.host.settings != { }) "${hostSettingsFile}")
            ++ (optional (cfg.host.environmentFile != null) "-${toString cfg.host.environmentFile}");
        };
      };
    })

    # --- client --------------------------------------------------------------------------------
    (mkIf cfg.client.enable {
      environment.systemPackages = [ cfg.client.package ];
      # 70-punktfunk-client.rules: hidraw access for the seated user's DualSense (SDL HIDAPI). The
      # rule is uaccess-tagged, so the active-seat user gets it with no group membership.
      services.udev.packages = [ cfg.client.package ];

      networking.firewall = mkIf cfg.client.openFirewall {
        allowedUDPPorts = [ 5353 ];
      };
    })
  ];
}
