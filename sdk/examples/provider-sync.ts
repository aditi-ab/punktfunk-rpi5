// ── Example 3/4 · typed bulk REST ───────────────────────────────────────────────────────────
// The external game-library provider pattern (RFC §8): compute your desired title list and
// declaratively reconcile it — the host diffs by your `external_id`, keeps host ids stable across
// syncs, drops orphans, and never touches manual entries. Uninstall = one call.
//
// Note how the typed `payload` catches shape mistakes at compile time: `launch` is a
// `LaunchSpec` — `{ kind, value }`, not a bare command string.
import { connect } from "../src/index.js";

const PROVIDER = "romm"; // your punktfunk-plugin-* name

const pf = await connect();
pf.events.on("library.changed", (e) => {
	if (e.source === PROVIDER) console.log("library synced");
});

// Fetch your source of truth (a ROM manager, itch.io, a curated list…), then reconcile:
const desired = [
	{
		external_id: "rom-1",
		title: "Chrono Trigger",
		launch: { kind: "command", value: "retroarch -L snes9x chrono-trigger.sfc" },
	},
	{
		external_id: "rom-2",
		title: "Super Metroid",
		launch: { kind: "command", value: "retroarch -L snes9x super-metroid.sfc" },
	},
];
const entries = await pf.api.reconcileProviderEntries(PROVIDER, { payload: desired });
console.log(
	`synced ${entries.length} titles:`,
	entries.map((e) => `${e.title} (custom:${e.id})`),
);

// …run on a schedule, or keep watching your source. Clean uninstall:
//   await pf.api.deleteProviderEntries(PROVIDER);
pf.close();
