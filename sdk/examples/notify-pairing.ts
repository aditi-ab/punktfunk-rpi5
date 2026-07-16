// The flagship hook-with-a-decision pattern (Promise facade): watch pairing requests, notify,
// and approve/deny THROUGH the API — asynchronously, the punktfunk way (hooks never veto).
import { connect } from "../src/index.js";

const pf = await connect();
console.log("watching for pairing requests…");

pf.events.on("pairing.pending", async (e) => {
	console.log(`pairing request: ${e.device.name} (${e.device.fingerprint})`);
	// Wire your real notifier here (ntfy, Pushover, Home Assistant, …), then decide:
	//   const pending = await pf.request("GET", "/native/pending") as { id: number }[];
	//   await pf.request("POST", `/native/pending/${pending[0].id}/approve`);
});
pf.events.on("pairing.completed", (e) => console.log(`paired: ${e.device.name}`));
pf.events.on("pairing.denied", (e) => console.log(`denied: ${e.device.name}`));
