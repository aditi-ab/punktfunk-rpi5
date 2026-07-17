// ── Example 1/4 · the "hello world" ─────────────────────────────────────────────────────────
// Connect, make one typed API call, and tail the host's lifecycle events. No Effect knowledge
// needed — `connect()` returns a plain Promise client.
//
//   bun examples/tail-events.ts          (on the host box: zero config)
//   PUNKTFUNK_MGMT_URL=… PUNKTFUNK_MGMT_TOKEN=… bun examples/tail-events.ts
import { connect } from "../src/index.js";

const pf = await connect();

// `pf.api.*` is the typed front door: `host` is a fully-typed HostInfo — no cast, autocomplete
// for every field.
const host = await pf.api.getHostInfo();
console.log(`connected to ${host.hostname} — tailing events (^C to stop)`);

pf.events.on("*", (e) => console.log(`[${e.seq}] ${e.kind}`, JSON.stringify(e)));
pf.events.on("unknown", (e) => console.log("[unknown kind]", JSON.stringify(e)));
pf.events.on("dropped", () => console.log("[cursor fell off the ring — resync]"));
