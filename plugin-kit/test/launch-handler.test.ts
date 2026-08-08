// The `/__launch` wire shape — the plugin half of the `plugin` launch kind, and a contract with the
// HOST (`library::ask_plugin_launch`), so it is driven end to end here rather than mocked.
import { describe, expect, test } from "bun:test";
import { Effect } from "effect";
import { makeLaunchHandler, type PluginLaunchTarget } from "../src/index.js";

const post = (body: unknown, init?: RequestInit): Request =>
	new Request("http://127.0.0.1/__launch", {
		method: "POST",
		headers: { "Content-Type": "application/json" },
		body: typeof body === "string" ? body : JSON.stringify(body),
		...init,
	});

/** A resolver that owns exactly one entry — the shape every real plugin's resolver has. */
const oneEntry = (key: string, target: PluginLaunchTarget) =>
	makeLaunchHandler((entry) => Effect.succeed(entry === key ? target : null));

describe("makeLaunchHandler", () => {
	test("answers a known entry with its command", async () => {
		const h = oneEntry("snes/smw.sfc", {
			command: "retroarch -L snes9x.so '/roms/snes/smw.sfc'",
		});
		const res = await h(post({ entry: "snes/smw.sfc" }));
		expect(res.status).toBe(200);
		expect(await res.json()).toEqual({
			command: "retroarch -L snes9x.so '/roms/snes/smw.sfc'",
		});
	});

	test("carries a working directory only when the plugin set one", async () => {
		const withCwd = oneEntry("k", { command: "run", cwd: "/opt/emu" });
		expect(await (await withCwd(post({ entry: "k" }))).json()).toEqual({
			command: "run",
			cwd: "/opt/emu",
		});
		const without = oneEntry("k", { command: "run" });
		// Absent, not `cwd: undefined` — the host decodes {command, cwd?} and a present-but-null key
		// is the exact shape that broke this plugin's own API once before (v0.3.2).
		expect(
			Object.hasOwn(await (await without(post({ entry: "k" }))).json(), "cwd"),
		).toBe(false);
	});

	test("404s an entry the plugin does not own — the forged-entry case", async () => {
		const h = oneEntry("mine", { command: "run" });
		const res = await h(post({ entry: "someone-elses" }));
		expect(res.status).toBe(404);
	});

	test("a resolver that dies is a 500, distinct from disowning the entry", async () => {
		const h = makeLaunchHandler(
			() => Effect.die(new Error("cache unreadable")) as Effect.Effect<null>,
		);
		const res = await h(post({ entry: "k" }));
		expect(res.status).toBe(500);
	});

	test("refuses a body that is not {entry: string}", async () => {
		const h = oneEntry("k", { command: "run" });
		expect((await h(post("not json at all"))).status).toBe(400);
		expect((await h(post({}))).status).toBe(400);
		expect((await h(post({ entry: 42 }))).status).toBe(400);
		expect((await h(post({ entry: "" }))).status).toBe(400);
	});

	test("only POST", async () => {
		const h = oneEntry("k", { command: "run" });
		const res = await h(
			new Request("http://127.0.0.1/__launch", { method: "GET" }),
		);
		expect(res.status).toBe(405);
	});
});
