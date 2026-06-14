import {
  ButtonItem,
  Field,
  PanelSection,
  PanelSectionRow,
  Spinner,
} from "@decky/ui";
import {
  callable,
  definePlugin,
  toaster,
} from "@decky/api";
import { useEffect, useState } from "react";
import { FaTv, FaSyncAlt, FaStop, FaLock, FaLockOpen } from "react-icons/fa";

// ---- Backend bridge (see main.py) ----

interface Host {
  name: string;
  host: string;
  port: number;
  pair: string; // "required" | "optional"
  fp: string;
}

interface ConnectResult {
  ok: boolean;
  host: string | null;
  error?: string;
}

interface Status {
  connected: boolean;
  host: string | null;
}

const discover = callable<[], Host[]>("discover");
const connect = callable<[host: string, port: number], ConnectResult>("connect");
const disconnect = callable<[], { ok: boolean; host: string | null }>("disconnect");
const getStatus = callable<[], Status>("status");

function Content() {
  const [hosts, setHosts] = useState<Host[]>([]);
  const [scanning, setScanning] = useState(false);
  const [busyHost, setBusyHost] = useState<string | null>(null);
  const [connectedHost, setConnectedHost] = useState<string | null>(null);

  const refresh = async () => {
    setScanning(true);
    try {
      const found = await discover();
      setHosts(found);
      toaster.toast({
        title: "punktfunk",
        body:
          found.length === 0
            ? "No hosts found on the LAN"
            : `Found ${found.length} host${found.length === 1 ? "" : "s"}`,
      });
    } catch (e) {
      toaster.toast({ title: "punktfunk", body: `Discovery failed: ${e}` });
    } finally {
      setScanning(false);
    }
  };

  const onConnect = async (h: Host) => {
    const target = `${h.host}:${h.port}`;
    setBusyHost(target);
    try {
      const res = await connect(h.host, h.port);
      if (res.ok) {
        setConnectedHost(res.host);
        toaster.toast({ title: "punktfunk", body: `Connecting to ${h.name}` });
      } else {
        toaster.toast({
          title: "punktfunk",
          body:
            res.error === "client-not-found"
              ? "punktfunk-client is not installed"
              : `Connect failed: ${res.error ?? "unknown"}`,
        });
      }
    } catch (e) {
      toaster.toast({ title: "punktfunk", body: `Connect failed: ${e}` });
    } finally {
      setBusyHost(null);
    }
  };

  const onDisconnect = async () => {
    try {
      await disconnect();
      setConnectedHost(null);
      toaster.toast({ title: "punktfunk", body: "Disconnected" });
    } catch (e) {
      toaster.toast({ title: "punktfunk", body: `Disconnect failed: ${e}` });
    }
  };

  // On panel open: sync the current connection status and do an initial scan.
  useEffect(() => {
    getStatus()
      .then((s) => setConnectedHost(s.connected ? s.host : null))
      .catch(() => {});
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <>
      <PanelSection title="Status">
        <PanelSectionRow>
          <Field label="State" focusable={false}>
            {connectedHost ? `Connected — ${connectedHost}` : "Idle"}
          </Field>
        </PanelSectionRow>
        {connectedHost && (
          <PanelSectionRow>
            <ButtonItem layout="below" onClick={onDisconnect}>
              <FaStop style={{ marginRight: "0.5em" }} />
              Disconnect
            </ButtonItem>
          </PanelSectionRow>
        )}
      </PanelSection>

      <PanelSection title="Hosts">
        <PanelSectionRow>
          <ButtonItem layout="below" onClick={refresh} disabled={scanning}>
            {scanning ? (
              <Spinner style={{ height: "1em", marginRight: "0.5em" }} />
            ) : (
              <FaSyncAlt style={{ marginRight: "0.5em" }} />
            )}
            {scanning ? "Scanning…" : "Refresh"}
          </ButtonItem>
        </PanelSectionRow>

        {hosts.length === 0 && !scanning && (
          <PanelSectionRow>
            <Field focusable={false}>No hosts discovered yet.</Field>
          </PanelSectionRow>
        )}

        {hosts.map((h) => {
          const target = `${h.host}:${h.port}`;
          const isBusy = busyHost === target;
          const pairRequired = h.pair === "required";
          return (
            <PanelSectionRow key={h.fp || target}>
              <ButtonItem
                layout="below"
                disabled={isBusy}
                onClick={() => onConnect(h)}
                label={
                  <span>
                    {pairRequired ? (
                      <FaLock style={{ marginRight: "0.4em" }} />
                    ) : (
                      <FaLockOpen style={{ marginRight: "0.4em" }} />
                    )}
                    {h.name}
                  </span>
                }
                description={`${target}${pairRequired ? " · pairing required" : ""}`}
              >
                {isBusy ? "Connecting…" : "Connect"}
              </ButtonItem>
            </PanelSectionRow>
          );
        })}
      </PanelSection>
    </>
  );
}

export default definePlugin(() => {
  return {
    name: "punktfunk",
    titleView: <div>punktfunk</div>,
    content: <Content />,
    icon: <FaTv />,
    onDismount() {
      // The backend tears the client down on _unload; nothing frontend-side to clean up.
    },
  };
});
