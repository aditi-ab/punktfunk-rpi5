// The per-host game picker + pinned-game launch helper. The picker fetches a paired
// host's library through the backend (headless flatpak --library — a cold client start
// can take seconds, hence the explicit spinner copy) and pins titles as one-tap rows in
// the QAM's Games section; its header also launches the GTK client's on-screen gamepad
// library (`--browse`).
import { DialogButton, Field, ModalRoot, Spinner, showModal } from "@decky/ui";
import { FC, useEffect, useState } from "react";
import { FaThLarge, FaTv } from "react-icons/fa";
import { GameEntry, Host, library, LibraryResult, PinnedGame } from "./backend";
import { PinsApi, resolvePinHost, startBrowse, startStream } from "./hooks";
import { isSafeLaunchId } from "./steam";
import { PairModal } from "./pair";
import { RowActions, actionButton } from "./ui";

/** Human store tag (mirrors the GTK client's `store_label`). */
export function storeLabel(store: string): string {
  switch (store) {
    case "steam":
      return "Steam";
    case "custom":
      return "Custom";
    case "heroic":
      return "Heroic";
    case "lutris":
      return "Lutris";
    case "epic":
      return "Epic";
    case "gog":
      return "GOG";
    case "xbox":
      return "Xbox";
    default:
      return "Game";
  }
}

/**
 * Stream a pinned game: resolve the host from the live scan (fp → id → stored address),
 * opportunistically refresh a drifted stored address, and route through pairing first if
 * this device is no longer paired with the host.
 */
export function streamPin(pin: PinnedGame, live: Host[], pins: PinsApi): void {
  const { host, online } = resolvePinHost(pin, live);
  if (online) {
    pins.updatePinHost(pin, host); // no-op unless the address actually drifted
  }
  if (!pin.paired) {
    showModal(
      <PairModal
        host={host}
        onPaired={() => {
          void pins.refresh(); // pick up the now-paired annotation
          void startStream(host, { launchId: pin.game_id }, pin.title);
        }}
      />,
    );
    return;
  }
  void startStream(host, { launchId: pin.game_id }, pin.title);
}

// Copy per backend error code (LibraryResult.error); `detail` covers the generic case.
function errorCopy(res: LibraryResult): string {
  switch (res.error) {
    case "not-paired":
      return "This Deck isn't paired with the host — pair first, then browse its library.";
    case "pin-mismatch":
      return "The host's identity changed — re-pair to re-establish trust.";
    case "unreachable":
      return "Couldn't reach the host's management API. Is the host online and up to date?";
    case "timeout":
      return "Timed out talking to the host — try again.";
    case "flatpak-not-found":
      return "The Punktfunk client isn't installed (flatpak io.unom.Punktfunk).";
    case "client-outdated":
      return "The installed client is too old for library browsing — update it from the About tab.";
    default:
      return res.detail || "Couldn't fetch the library.";
  }
}

// ----------------------------------------------------------------------------------------
// The picker modal: "open on screen" + a pin-toggle list of the host's games.
// ----------------------------------------------------------------------------------------
export const GamePickerModal: FC<{
  host: Host;
  pins: PinsApi;
  clientUpdatePending?: boolean;
  closeModal?: () => void;
}> = ({ host, pins, clientUpdatePending, closeModal }) => {
  const [result, setResult] = useState<LibraryResult | null>(null);
  const [attempt, setAttempt] = useState(0); // bump to refetch (retry / after pairing)
  // The modal is a detached `showModal` portal that never re-renders from the page's pin
  // state, so `pins.isPinned` would read a frozen snapshot and the Pin/Unpin label would
  // never flip within a session. Track this host's pinned ids locally, seeded once from the
  // snapshot at open; persistence still goes through the (stale-closure-safe) pins API.
  const [pinnedIds, setPinnedIds] = useState<Set<string>>(
    () => new Set(pins.pins.filter((p) => p.host_fp === host.fp).map((p) => p.game_id)),
  );
  const togglePin = (g: GameEntry) => {
    const wasPinned = pinnedIds.has(g.id);
    setPinnedIds((prev) => {
      const next = new Set(prev);
      if (wasPinned) next.delete(g.id);
      else next.add(g.id);
      return next;
    });
    if (wasPinned) pins.removePin(host.fp, g.id);
    else pins.addPin(host, g);
  };

  useEffect(() => {
    let stale = false;
    setResult(null);
    library(host.host, host.mgmt, host.fp)
      .then((res) => {
        if (!stale) setResult(res);
      })
      .catch((e) => {
        if (!stale) setResult({ ok: false, error: "client-error", detail: String(e) });
      });
    return () => {
      stale = true;
    };
  }, [host.host, host.mgmt, host.fp, attempt]);

  const games = (result?.ok && result.games) || [];
  const sorted = [...games].sort((a, b) => a.title.localeCompare(b.title));

  return (
    <ModalRoot closeModal={closeModal}>
      <div style={{ fontWeight: "bold", fontSize: "1.3em", marginBottom: "0.4em" }}>
        {host.name} — Games
      </div>

      <Field
        label="Open library on screen"
        description="Browse this host's games with the controller, full screen"
        childrenContainerWidth="max"
      >
        <RowActions>
          <DialogButton
            style={actionButton}
            onClick={() => {
              closeModal?.();
              void startBrowse(host);
            }}
          >
            <FaTv style={{ marginRight: "0.4em" }} />
            Open
          </DialogButton>
        </RowActions>
      </Field>

      {clientUpdatePending && (
        <Field
          focusable={false}
          description="A client update is available — direct game launch and on-screen browsing need the latest client."
        />
      )}

      {result === null && (
        <Field
          focusable={false}
          label={
            <span style={{ display: "inline-flex", alignItems: "center", gap: "0.6em" }}>
              <Spinner style={{ height: "1em" }} />
              Fetching the library…
            </span>
          }
          description="This starts the client headlessly — a cold start can take a few seconds."
        />
      )}

      {result !== null && !result.ok && (
        <Field label="Couldn't fetch the library" description={errorCopy(result)} childrenContainerWidth="max">
          <RowActions>
            {result.error === "not-paired" && (
              <DialogButton
                style={actionButton}
                onClick={() =>
                  showModal(<PairModal host={host} onPaired={() => setAttempt((n) => n + 1)} />)
                }
              >
                Pair
              </DialogButton>
            )}
            <DialogButton style={actionButton} onClick={() => setAttempt((n) => n + 1)}>
              Retry
            </DialogButton>
          </RowActions>
        </Field>
      )}

      {result?.ok && sorted.length === 0 && (
        <Field
          focusable={false}
          label="No games found"
          description="Install Steam titles or add custom entries in the host's web console."
        />
      )}

      {sorted.length > 0 && (
        <div style={{ maxHeight: "55vh", overflowY: "auto" }}>
          {sorted.map((g: GameEntry) => {
            const pinned = pinnedIds.has(g.id);
            const safe = isSafeLaunchId(g.id);
            return (
              <Field
                key={g.id}
                label={g.title}
                description={
                  storeLabel(g.store) + (safe ? "" : " · unsupported id — can't be pinned")
                }
                childrenContainerWidth="max"
              >
                <RowActions>
                  <DialogButton style={actionButton} disabled={!safe} onClick={() => togglePin(g)}>
                    <FaThLarge style={{ marginRight: "0.4em" }} />
                    {pinned ? "Unpin" : "Pin"}
                  </DialogButton>
                </RowActions>
              </Field>
            );
          })}
        </div>
      )}
    </ModalRoot>
  );
};
