// Add / edit host dialogs for the fullscreen page. These mutate the SHARED known-hosts store
// (client-known-hosts.json) through the flatpak client's headless modes, so a host saved or
// renamed here shows up in the desktop client too. Text entry uses @decky/ui's TextField, which
// brings up Steam's on-screen keyboard on focus (the digit-grid trick in pair.tsx is only needed
// for the numeric PIN).
import { DialogButton, Focusable, ModalRoot, Spinner, TextField } from "@decky/ui";
import { toaster } from "@decky/api";
import { ChangeEvent, FC, useState } from "react";
import { addHost, editHost, MutationResult } from "./backend";
import { HostView } from "./hooks";
import { actionButton } from "./ui";

/** Stable copy for a failed host-store mutation. */
export function mutationError(r: MutationResult): string {
  switch (r.error) {
    case "client-unavailable":
      return "The Punktfunk client isn't installed (flatpak io.unom.Punktfunk).";
    case "client-outdated":
      return "The installed client is too old for host management — update it from the About tab.";
    default:
      return r.detail || "Couldn't save the host.";
  }
}

// Split a typed address: a pasted `host:port` wins over the separate port field. IPv6 literals
// aren't supported by the host advert/known-hosts format, so a bare colon is treated as host:port.
function targetFrom(addr: string, port: string): string {
  const a = addr.trim();
  if (a.includes(":")) {
    return a;
  }
  const p = port.trim() || "9777";
  return `${a}:${p}`;
}

const field: React.CSSProperties = { marginBottom: "0.8em" };

const HostForm: FC<{
  title: string;
  submitLabel: string;
  initial: { addr: string; port: string; name: string };
  addrDisabled?: boolean;
  onSubmit: (addr: string, port: string, name: string) => Promise<MutationResult>;
  onDone: () => void;
  closeModal?: () => void;
}> = ({ title, submitLabel, initial, addrDisabled, onSubmit, onDone, closeModal }) => {
  const [addr, setAddr] = useState(initial.addr);
  const [port, setPort] = useState(initial.port);
  const [name, setName] = useState(initial.name);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    if (!addr.trim()) {
      setError("Enter an address.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const r = await onSubmit(addr.trim(), port.trim(), name.trim());
      if (r.ok) {
        onDone();
        closeModal?.();
      } else {
        setError(mutationError(r));
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalRoot closeModal={closeModal}>
      <div style={{ fontWeight: "bold", fontSize: "1.3em", marginBottom: "0.6em" }}>{title}</div>
      <div style={field}>
        <TextField
          label="Address"
          description="IP or hostname (a Tailscale/VPN name works too). Add :port to override."
          value={addr}
          disabled={addrDisabled || busy}
          onChange={(e: ChangeEvent<HTMLInputElement>) => setAddr(e.target.value)}
        />
      </div>
      <div style={field}>
        <TextField
          label="Port"
          value={port}
          mustBeNumeric
          disabled={busy}
          onChange={(e: ChangeEvent<HTMLInputElement>) => setPort(e.target.value)}
        />
      </div>
      <div style={field}>
        <TextField
          label="Name (optional)"
          value={name}
          disabled={busy}
          onChange={(e: ChangeEvent<HTMLInputElement>) => setName(e.target.value)}
        />
      </div>
      {error && (
        <div style={{ color: "#ff6b6b", marginBottom: "0.6em" }}>{error}</div>
      )}
      <Focusable style={{ display: "flex", gap: "0.5em", justifyContent: "flex-end" }}>
        <DialogButton style={actionButton} disabled={busy} onClick={() => closeModal?.()}>
          Cancel
        </DialogButton>
        <DialogButton style={actionButton} disabled={busy} onClick={submit}>
          {busy ? <Spinner style={{ height: "1em" }} /> : submitLabel}
        </DialogButton>
      </Focusable>
    </ModalRoot>
  );
};

/** "+" — save a new host by address (unpaired placeholder; the user pairs it next). */
export const AddHostModal: FC<{ onDone: () => void; closeModal?: () => void }> = ({
  onDone,
  closeModal,
}) => (
  <HostForm
    title="Add host"
    submitLabel="Add"
    initial={{ addr: "", port: "9777", name: "" }}
    onSubmit={async (addr, port, name) => {
      const r = await addHost(targetFrom(addr, port), name, "");
      if (r.ok) {
        toaster.toast({ title: "Punktfunk", body: `Added ${name || addr}` });
      }
      return r;
    }}
    onDone={onDone}
    closeModal={closeModal}
  />
);

/** Rename / re-point a saved host. Identified by fingerprint when it has one (survives IP
 *  changes), else by its current address. */
export const EditHostModal: FC<{
  host: HostView;
  onDone: () => void;
  closeModal?: () => void;
}> = ({ host, onDone, closeModal }) => {
  const selector = host.fp || `${host.addr}:${host.port}`;
  return (
    <HostForm
      title={`Edit ${host.name}`}
      submitLabel="Save"
      initial={{ addr: host.addr, port: String(host.port), name: host.name }}
      onSubmit={async (addr, port, name) => {
        const r = await editHost(selector, name, addr, parseInt(port, 10) || 0);
        if (r.ok) {
          toaster.toast({ title: "Punktfunk", body: `Updated ${name || addr}` });
        }
        return r;
      }}
      onDone={onDone}
      closeModal={closeModal}
    />
  );
};
