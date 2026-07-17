// ── Example 2/4 · event → decision ──────────────────────────────────────────────────────────
// The flagship pattern: watch pairing requests, notify yourself, and approve/deny THROUGH the
// typed API — asynchronously, the punktfunk way (hooks never veto). Every call is checked and
// autocompleted; no hand-written paths, no casts.
import { connect } from "../src/index.js";

const pf = await connect();
console.log("watching for pairing requests…");

pf.events.on("pairing.pending", async (e) => {
	console.log(`pairing request: ${e.device.name} (${e.device.fingerprint})`);

	// Wire your real notifier here (ntfy, Pushover, Home Assistant, …). Then decide by calling
	// the API — find the pending device by fingerprint and approve it (send `{}` to keep the
	// name it knocked with):
	const pending = await pf.api.listPendingDevices();
	const match = pending.find((d) => d.fingerprint === e.device.fingerprint);
	if (match) await pf.api.approvePendingDevice(String(match.id), { payload: {} });
});
pf.events.on("pairing.completed", (e) => console.log(`paired: ${e.device.name}`));
pf.events.on("pairing.denied", (e) => console.log(`denied: ${e.device.name}`));
