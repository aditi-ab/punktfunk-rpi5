// Tail the host's lifecycle events — the SDK "hello world" (Promise facade).
//
//   bun examples/tail-events.ts          (on the host box: zero config)
//   PUNKTFUNK_MGMT_URL=… PUNKTFUNK_MGMT_TOKEN=… bun examples/tail-events.ts
import { connect } from "../src/index.js";

const pf = await connect();
const host = (await pf.request("GET", "/host")) as { hostname?: string };
console.log(`connected to ${host.hostname ?? "host"} — tailing events (^C to stop)`);

pf.events.on("*", (e) => console.log(`[${e.seq}] ${e.kind}`, JSON.stringify(e)));
pf.events.on("unknown", (e) => console.log("[unknown kind]", JSON.stringify(e)));
pf.events.on("dropped", () => console.log("[cursor fell off the ring — resync]"));
