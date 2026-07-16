// The external game-library provider pattern (RFC §8): compute your desired title list and
// declaratively PUT it — the host diffs by your `external_id`, keeps host ids stable across
// syncs, drops orphans, and never touches manual entries. Uninstall = one DELETE.
import { connect } from "../src/index.js";

const PROVIDER = "romm"; // your punktfunk-plugin-* name

const pf = await connect();
pf.events.on("library.changed", (e) => {
	if (e.source === PROVIDER) console.log("library synced");
});

// Fetch your source of truth (a ROM manager, itch.io, a curated list…), then reconcile:
const desired = [
	{ external_id: "rom-1", title: "Chrono Trigger", launch: { command: "retroarch ..." } },
	{ external_id: "rom-2", title: "Super Metroid", launch: { command: "retroarch ..." } },
];
const entries = (await pf.request("PUT", `/library/provider/${PROVIDER}`, desired)) as {
	id: string;
	title: string;
}[];
console.log(`synced ${entries.length} titles:`, entries.map((e) => `${e.title} (custom:${e.id})`));

// …run on a schedule, or keep watching your source. Clean uninstall:
//   await pf.request("DELETE", `/library/provider/${PROVIDER}`);
pf.close();
