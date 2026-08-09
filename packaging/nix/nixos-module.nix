# NixOS integration for punktfunk — the declarative equivalent of everything the RPM/deb do in
# their %install + %post (packaging/rpm/punktfunk.spec, packaging/debian/build-deb.sh):
# the systemd *user* service, the uinput/uhid/vhci udev rules, the vhci-hcd autoload, the 32 MB
# UDP socket-buffer sysctls, the firewall openers, the `input`- and `punktfunk`-group membership
# for virtual gamepads, the management web console (`services.punktfunk.web`, on by default with
# the host — the RPM/deb Recommends), and the opt-in plugin/script runner
# (`services.punktfunk.scripting`).
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
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib)
    mkEnableOption
    mkOption
    mkIf
    mkMerge
    mkDefault
    types
    optional
    optionals
    optionalString
    literalExpression
    concatStringsSep
    mapAttrsToList
    genAttrs
    ;

  cfg = config.services.punktfunk;
  system = pkgs.stdenv.hostPlatform.system;

  # host.env rendering: booleans → 1/0 (what PUNKTFUNK_* knobs expect), everything else verbatim.
  renderVal = v: if lib.isBool v then (if v then "1" else "0") else toString v;
  renderEnv =
    attrs: concatStringsSep "\n" (mapAttrsToList (k: v: "${k}=${renderVal v}") attrs) + "\n";

  hostSettingsFile = pkgs.writeText "punktfunk-host.env" (renderEnv cfg.host.settings);

  # Native punktfunk/1 ports (control plane + discovery + mgmt API). The media data plane is an
  # ephemeral per-session UDP port the host hole-punches, so nothing fixed to open (see
  # packaging/linux/punktfunk.ufw).
  nativeTCP = [ 47990 ]; # mgmt/library REST API (HTTPS + mTLS)
  nativeUDP = [
    9777
    5353
  ]; # QUIC control plane + mDNS
  # GameStream/Moonlight-compat fixed ports (opt-in with `host.gamestream`).
  gamestreamTCP = [
    47984
    47989
    48010
  ];
  gamestreamUDP = [
    47998
    47999
    48000
  ];
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
          Users to add to the `input` and `punktfunk` groups — required for the virtual gamepads
          the host creates: `input` covers `/dev/uinput` and `/dev/uhid`, `punktfunk` covers the
          usbip/vhci nodes the virtual Steam Deck pad attaches through. The second is separate on
          purpose — it can emulate arbitrary USB hardware, so only list users you would trust with
          that. The host runs as these users' `systemd --user` service.
        '';
      };

      settings = mkOption {
        type = types.attrsOf (
          types.oneOf [
            types.str
            types.int
            types.bool
          ]
        );
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

      gamescopeHdr = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Put `punktfunk-gamescope` — gamescope carrying punktfunk's `pipewire-hdr` patches — on
          the host service's PATH, so a 10-bit-capable client can stream true HDR10 (BT.2020 PQ)
          off a gamescope virtual output. HDR is attempted by default once the binary is present
          (`PUNKTFUNK_GAMESCOPE_HDR=0` in `settings`/`environmentFile` forces SDR).

          It does NOT replace the system's `gamescope`: the binary has its own name and the host
          prefers it only for the sessions it spawns itself. Costs a gamescope build from source
          — set `false` to skip that build; the host then stays SDR on the gamescope backend.
        '';
      };

      gamescopePackage = mkOption {
        type = types.package;
        default = self.packages.${system}.punktfunk-gamescope;
        defaultText = literalExpression "punktfunk.packages.\${system}.punktfunk-gamescope";
        description = ''
          The patched gamescope used when `gamescopeHdr = true`. Override to build it from a
          different nixpkgs (the patches apply to whatever gamescope that nixpkgs pins).
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

    # The management web console (SPAKE2 PIN pairing + host status) — the browser UI every client
    # needs. Ships by DEFAULT alongside the host (mirrors the RPM's `Recommends: punktfunk-web` and
    # the .deb the host package pulls in), auto-wired to the host's mgmt token + identity cert.
    web = {
      enable = mkOption {
        type = types.bool;
        default = cfg.host.enable;
        defaultText = literalExpression "config.services.punktfunk.host.enable";
        description = ''
          Run the management web console as a `systemd --user` service on TCP 47992 (HTTPS). Enabled
          by default whenever the host is enabled — set to `false` for a console-less host. It
          auto-wires to `~/.config/punktfunk/{mgmt-token,cert.pem,key.pem}` (written by the host's
          `serve`) and generates a login password on first start.
        '';
      };

      package = mkOption {
        type = types.package;
        default = self.packages.${system}.punktfunk-web;
        defaultText = literalExpression "punktfunk.packages.\${system}.punktfunk-web";
        description = "The punktfunk-web package (the bun-built Nitro SSR console bundle).";
      };

      openFirewall = mkOption {
        type = types.bool;
        default = cfg.host.openFirewall;
        defaultText = literalExpression "config.services.punktfunk.host.openFirewall";
        description = ''
          Open TCP 47992 so the console is reachable from other devices on the LAN, and TCP 47993,
          the separate origin its plugin UIs are served from (without it, plugin interfaces do not
          load in the console).
        '';
      };

      autoStart = mkOption {
        type = types.bool;
        default = cfg.host.autoStart;
        defaultText = literalExpression "config.services.punktfunk.host.autoStart";
        description = ''
          Start the console automatically in every user's graphical session (adds it to the user
          `default.target`). Follows the host's `autoStart` by default — for a login-less appliance,
          enable lingering for the user as well.
        '';
      };
    };

    # The plugin/script runner — host automation on bun. Ships with the host (the RPM/deb Recommends
    # it), but running it is OPT-IN: the `systemd --user` unit is defined yet NOT added to
    # `default.target`, because the runner is inert until you add scripts/plugins. Turn it on with
    # `systemctl --user enable --now punktfunk-scripting`.
    scripting = {
      enable = mkOption {
        type = types.bool;
        default = cfg.host.enable;
        defaultText = literalExpression "config.services.punktfunk.host.enable";
        description = ''
          Install the plugin/script runner and define its `systemd --user` unit
          (`punktfunk-scripting`). Enabled by default whenever the host is — but the unit is not
          auto-started (see `autoStart`), since the runner does nothing until you add scripts to
          `~/.config/punktfunk/scripts` or install `punktfunk-plugin-*` packages under
          `~/.config/punktfunk/plugins`. A plugin auto-wires to the host's mgmt token + identity cert.
        '';
      };

      package = mkOption {
        type = types.package;
        default = self.packages.${system}.punktfunk-scripting;
        defaultText = literalExpression "punktfunk.packages.\${system}.punktfunk-scripting";
        description = "The punktfunk-scripting package (the bun-bundled Effect SDK runner).";
      };

      autoStart = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Start the runner automatically in every user's graphical session (adds it to the user
          `default.target`). Off by default even when the host auto-starts — running arbitrary
          operator scripts/plugins is a deliberate opt-in; enable it once you have automation to run.
        '';
      };
    };
  };

  config = mkMerge [
    # --- shared: whenever either half is enabled -----------------------------------------------
    (mkIf (cfg.host.enable || cfg.client.enable) {
      assertions = [
        {
          assertion = system == "x86_64-linux";
          message = "services.punktfunk is x86_64-linux only (desktop NVENC host; no aarch64 build).";
        }
      ];
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
      boot.kernelModules = [
        "uinput"
        "uhid"
        "vhci-hcd"
      ];

      # `input` group membership for the virtual-gamepad nodes (mirrors the RPM's usermod hint).
      #
      # `punktfunk` is the SECOND group 60-punktfunk.rules needs: it owns the usbip vhci
      # attach/detach nodes, and is deliberately not `input` because writing `attach` materialises
      # an arbitrary emulated USB device — a root-only kernel primitive that must not ride on the
      # group every gamepad guide tells you to join (security-review 2026-08-05 M-4). Declaring the
      # group is not optional: the rule shells out to `chgrp punktfunk`, which fails outright if
      # nothing ever created it, leaving the nodes root-only and the virtual Steam Deck pad unable
      # to attach. Membership follows `host.users`, which is already the explicit "these users run
      # the host" list this option's description scopes to the usbip/vhci pad.
      users.groups.input = { };
      users.groups.punktfunk = { };
      users.users = genAttrs cfg.host.users (_: {
        extraGroups = [
          "input"
          "punktfunk"
        ];
      });

      # Status-tray autostart entry (self-gating: `--autostart` exits unless this user runs a host).
      environment.etc."xdg/autostart/io.unom.Punktfunk.Tray.desktop".source =
        "${cfg.host.package}/etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop";

      networking.firewall = mkIf cfg.host.openFirewall {
        allowedTCPPorts = nativeTCP ++ optionals cfg.host.gamestream gamestreamTCP;
        allowedUDPPorts = nativeUDP ++ optionals cfg.host.gamestream gamestreamUDP;
      };

      # NO CAP_SYS_NICE wrapper here — deliberately. 0.26.0-1 gave the host a
      # `security.wrappers.punktfunk-host` carrying `cap_sys_nice=ep` for the GPU-priority lever,
      # and that broke desktop streaming on every KDE box.
      #
      # KWin advertises its restricted Wayland protocols (zkde_screencast_unstable_v1 for the
      # virtual output, org_kde_kwin_fake_input for input) only to a client it can IDENTIFY, by
      # resolving that client's /proc/<pid>/exe and matching it against an installed .desktop's
      # Exec= (packages.nix substitutes ours to the store path). The kernel refuses that readlink to
      # any reader whose effective set is not a superset of the target's PERMITTED set
      # (cap_ptrace_access_check), and KWin holds no capabilities.
      #
      # A NixOS wrapper does not dodge this. It raises the capability into its AMBIENT set before
      # exec'ing the store binary, precisely so the capability survives — which lands CAP_SYS_NICE
      # in the exec'd process's permitted set and fails the readlink identically. Measured: an
      # ambient-only grant (dumpable=1, CapPrm set) is refused exactly like a file capability. See
      # packaging/arch/punktfunk-host.install for the full matrix.
      #
      # Costs pacing only: pf-zerocopy walks REALTIME -> HIGH -> default when a priority class is
      # refused, and pf-frame's thread nice is a best-effort no-op — 0.25.0's behaviour exactly.

      systemd.user.services.punktfunk-host = {
        description = "punktfunk GameStream + punktfunk/1 streaming host";
        documentation = [ "https://git.unom.io/unom/punktfunk" ];
        # Soft ordering: the host listens immediately and only touches the compositor per session.
        after = [ "pipewire.service" ];
        wants = [ "pipewire.service" ];
        wantedBy = optional cfg.host.autoStart "default.target";
        # The host may exec external helpers (pw-dump, sh, and — for the gamescope/kwin backends —
        # the compositor). Extend this in your config for a headless gamescope/KWin appliance.
        path = [
          pkgs.bash
          pkgs.coreutils
          pkgs.pipewire
        ]
        # The HDR-capable gamescope, if enabled. On PATH rather than pinned through
        # PUNKTFUNK_GAMESCOPE_BIN so an operator's own override of that env still wins.
        ++ optional cfg.host.gamescopeHdr cfg.host.gamescopePackage;
        serviceConfig = {
          # The store path DIRECTLY — not a capability wrapper. /proc/<pid>/exe then resolves to the
          # very path packages.nix substituted into io.unom.Punktfunk.Host.desktop's Exec=, which is
          # what lets KWin identify the host and grant it the screencast/fake-input protocols (see
          # the note above the firewall block).
          ExecStart =
            "${cfg.host.package}/bin/punktfunk-host serve" + optionalString cfg.host.gamestream " --gamestream";
          Restart = "on-failure";
          RestartSec = 2;
          EnvironmentFile =
            (optional (cfg.host.settings != { }) "${hostSettingsFile}")
            ++ (optional (cfg.host.environmentFile != null) "-${toString cfg.host.environmentFile}");
        };
        # No PUNKTFUNK_GAMESCOPE_HDR here: the host defaults it on, and the capability probe on
        # the resolved binary keeps a build-less box SDR. `gamescopeHdr` only controls whether
        # the patched binary is on PATH; `settings`/`environmentFile` can still set =0 to force SDR.
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

    # --- web console ---------------------------------------------------------------------------
    # The declarative equivalent of the punktfunk-web .deb / RPM subpackage: the two systemd --user
    # units (the console + its first-run password generator) plus the firewall opener, all auto-wired
    # to the host's per-user mgmt token + identity cert (no env editing on a packaged install).
    (mkIf cfg.web.enable {
      environment.systemPackages = [ cfg.web.package ];

      networking.firewall = mkIf cfg.web.openFirewall {
        # 47992 = the console itself. 47993 = the SEPARATE ORIGIN its plugin UIs are served from
        # (console port + 1): same host and certificate, different port, so the browser's same-origin
        # policy keeps a plugin from acting as the logged-in operator. Leaving it closed does not
        # degrade gracefully — every plugin interface is simply an empty panel from any other device.
        # Keep in step with packaging/linux/punktfunk-web.xml and punktfunk.ufw.
        allowedTCPPorts = [ 47992 47993 ];
      };

      # First-run setup: generate the console login password once, in the user's config dir, and
      # surface it to the --user journal. Self-gates via ConditionPathExists (mirrors
      # scripts/punktfunk-web-init.service).
      systemd.user.services.punktfunk-web-init = {
        description = "punktfunk web console first-run setup (login password)";
        documentation = [ "https://git.unom.io/unom/punktfunk" ];
        unitConfig.ConditionPathExists = "!%h/.config/punktfunk/web-password";
        path = [ pkgs.coreutils ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${cfg.web.package}/share/punktfunk-web/web-init.sh";
        };
      };

      # The console itself: Nitro SSR on bun, HTTPS on 47992 with the host's identity cert, proxying
      # the host's loopback mgmt API with the bearer token injected server-side. mgmt-token is
      # REQUIRED (the host's `serve` writes it) — if absent the unit fails and Restart retries until
      # the host has created it; web-password is optional ('-'). Mirrors scripts/punktfunk-web.service.
      systemd.user.services.punktfunk-web = {
        description = "punktfunk management web console";
        documentation = [ "https://git.unom.io/unom/punktfunk" ];
        after = [
          "punktfunk-web-init.service"
          "punktfunk-host.service"
        ];
        wants = [ "punktfunk-web-init.service" ];
        wantedBy = optional cfg.web.autoStart "default.target";
        environment = {
          PUNKTFUNK_MGMT_URL = "https://127.0.0.1:47990";
          PORT = "47992";
          HOST = "0.0.0.0";
          # Serve HTTPS with the host's own identity cert (the anchor native clients already pin) and
          # mark the session cookie Secure. The host's `serve` writes these PEMs.
          PUNKTFUNK_UI_TLS_CERT = "%h/.config/punktfunk/cert.pem";
          PUNKTFUNK_UI_TLS_KEY = "%h/.config/punktfunk/key.pem";
          PUNKTFUNK_UI_SECURE = "1";
        };
        serviceConfig = {
          Type = "simple";
          EnvironmentFile = [
            "%h/.config/punktfunk/mgmt-token"
            "-%h/.config/punktfunk/web-password"
          ];
          ExecStart = "${cfg.web.package}/bin/punktfunk-web-server";
          Restart = "on-failure";
          RestartSec = 2;
        };
      };
    })

    # --- plugin/script runner ------------------------------------------------------------------
    # Installs the runner + defines its opt-in `systemd --user` unit (mirrors the deb/rpm
    # punktfunk-scripting subpackage). NOT auto-started unless `scripting.autoStart` is set.
    (mkIf cfg.scripting.enable {
      environment.systemPackages = [ cfg.scripting.package ];

      systemd.user.services.punktfunk-scripting = {
        description = "punktfunk plugin/script runner";
        documentation = [ "https://git.unom.io/unom/punktfunk" ];
        # Plugins talk to the host's loopback mgmt API; order after it (soft — the runner backs off
        # and retries per unit, so this is ordering only, not a hard requirement).
        after = [ "punktfunk-host.service" ];
        wantedBy = optional cfg.scripting.autoStart "default.target";
        serviceConfig = {
          Type = "simple";
          ExecStart = "${cfg.scripting.package}/bin/punktfunk-scripting";
          Restart = "on-failure";
          RestartSec = 2;
          # Deliver SIGTERM to the runner (it orchestrates the structural shutdown of its unit
          # fibers) and give it room to run their finalizers before the cgroup is reaped.
          KillMode = "mixed";
          KillSignal = "SIGTERM";
          TimeoutStopSec = 30;
        };
      };
    })
  ];
}
