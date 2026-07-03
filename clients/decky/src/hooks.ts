// Shared state hooks + user actions for the QAM panel and the fullscreen page.
import { toaster } from "@decky/api";
import { Navigation } from "@decky/ui";
import { useCallback, useEffect, useState } from "react";
import { checkUpdate, discover, Host, updateClient, UpdateInfo } from "./backend";
import { launchStream } from "./steam";

export const DOCS_URL = "https://docs.punktfunk.unom.io/docs/steam-deck";

// Decky Loader exposes its already-authenticated WSRouter as a global. This is NOT part of
// @decky/api (it's a loader internal), so we treat it as optional and guard every use — on a
// loader without it we fall back to manual "Install Plugin from URL". We use it to drive
// Decky's own privileged install path (the root loader does the download + SHA-256 verify +
// extract + hot-reload), which is the only way a plugin can update itself: ~/homebrew/plugins
// is root-owned, so our unprivileged backend can't swap its own files.
declare global {
  interface Window {
    DeckyBackend?: {
      callable: (route: string) => (...args: unknown[]) => Promise<unknown>;
    };
  }
}

// PluginInstallType.UPDATE in decky-loader's browser.py (INSTALL=0/REINSTALL=1/UPDATE=2/…).
const INSTALL_TYPE_UPDATE = 2;

// ----------------------------------------------------------------------------------------
// Discovery — mDNS scan state shared by the QAM panel and the full page.
// ----------------------------------------------------------------------------------------
export function useHosts() {
  const [hosts, setHosts] = useState<Host[]>([]);
  const [scanning, setScanning] = useState(false);

  const refresh = useCallback(async () => {
    setScanning(true);
    try {
      setHosts(await discover());
    } catch (e) {
      toaster.toast({ title: "Punktfunk", body: `Discovery failed: ${e}` });
    } finally {
      setScanning(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return { hosts, scanning, refresh };
}

// ----------------------------------------------------------------------------------------
// Self-update — checks our registry on mount (the backend caches for 30 min + is non-fatal
// offline); `check(true)` bypasses the cache for the explicit "Check for updates" button.
// ----------------------------------------------------------------------------------------
export function useUpdate() {
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [checking, setChecking] = useState(false);

  const check = useCallback(async (force: boolean): Promise<UpdateInfo | null> => {
    setChecking(true);
    try {
      const res = await checkUpdate(force);
      setInfo(res);
      return res;
    } catch {
      return null;
    } finally {
      setChecking(false);
    }
  }, []);

  useEffect(() => {
    void check(false);
  }, [check]);

  return { info, checking, check };
}

/** True when EITHER the plugin or the flatpak client has a pending update. */
export function hasUpdate(info: UpdateInfo | null | undefined): boolean {
  return !!info && (info.update_available || info.client_update_available);
}

/** The explicit "Check for updates" action — always ends in a toast so the tap has feedback. */
export async function checkForUpdatesNow(
  check: (force: boolean) => Promise<UpdateInfo | null>,
): Promise<void> {
  const res = await check(true);
  let body: string;
  if (!res || res.error === "fetch-failed") {
    body = "Couldn’t reach the update server — are you online?";
  } else if (hasUpdate(res)) {
    const parts: string[] = [];
    if (res.update_available) parts.push(`plugin v${res.current} → v${res.latest}`);
    if (res.client_update_available) parts.push("client");
    body = `Update available: ${parts.join(" + ")}.`;
  } else if (res.error === "update-channel-unknown") {
    body = "Development build — plugin updates are disabled; the client is up to date.";
  } else {
    body = `You’re up to date (plugin v${res.current}).`;
  }
  toaster.toast({ title: "Punktfunk", body });
}

/**
 * Apply whichever updates are pending. The flatpak CLIENT is updated first (a user-scope
 * `flatpak update`, awaited); then, if the PLUGIN itself has an update, Decky's install RPC
 * reinstalls it — which reloads the plugin and tears this panel down, so it goes last and is
 * fire-and-forget. `check` (when passed) refreshes the panel state after a client-only update so
 * the "Update available" button clears.
 */
export async function applyUpdate(
  info: UpdateInfo,
  check?: (force: boolean) => Promise<UpdateInfo | null>,
): Promise<void> {
  if (info.client_update_available) {
    toaster.toast({ title: "Punktfunk", body: "Updating the client…" });
    try {
      const r = await updateClient();
      toaster.toast({
        title: "Punktfunk",
        body: !r.ok
          ? `Client update failed${r.error ? ` (${r.error})` : ""}.`
          : r.updated
            ? "Client updated to the latest version."
            : "Client is already up to date.",
      });
    } catch {
      toaster.toast({ title: "Punktfunk", body: "Client update failed." });
    }
  }

  if (info.update_available) {
    try {
      const backend = window.DeckyBackend;
      if (backend?.callable) {
        // Fire-and-forget: the loader reinstalls + reloads THIS plugin, tearing the panel down
        // before any result could arrive — so never await it. Decky shows its own confirm prompt.
        void backend.callable("utilities/install_plugin")(
          info.artifact,
          "punktfunk",
          info.latest,
          info.hash,
          INSTALL_TYPE_UPDATE,
        );
        toaster.toast({
          title: "Punktfunk",
          // Decky's installer also phones the plugin store first, which can hang on some
          // networks before the actual install proceeds — set expectations.
          body: `Updating the plugin to v${info.latest} — confirm Decky’s prompt. This can take a couple of minutes.`,
        });
        return;
      }
    } catch {
      // fall through to the manual path
    }
    toaster.toast({
      title: "Punktfunk",
      body: "Update the plugin from Decky → Developer → Install Plugin from URL.",
    });
    return;
  }

  // Client-only update (no plugin reinstall): refresh so the button clears.
  if (check) void check(true);
}

// ----------------------------------------------------------------------------------------
// Stream launch — via the hidden Steam shortcut (see steam.ts for why).
// ----------------------------------------------------------------------------------------
export async function startStream(h: Host): Promise<void> {
  try {
    await launchStream(h.host, h.port);
    Navigation.CloseSideMenus();
    toaster.toast({ title: "Punktfunk", body: `Starting stream — ${h.name}` });
  } catch (e) {
    toaster.toast({ title: "Punktfunk", body: `Launch failed: ${e}` });
  }
}
