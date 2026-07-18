// The runner's contract: discovery (scripts + plugin packages + the sshd refusal), both plugin
// shapes running against a mock host, crash → supervised restart, one-shot scripts, and the
// M5 heart — structured interruption running scoped finalizers on shutdown.
import { afterAll, describe, expect, test } from "bun:test";
import { Effect, Fiber } from "effect";
import * as fs from "node:fs";
import * as path from "node:path";
import { discoverUnits, runner, superviseUnit } from "../src/runner.js";

const TOKEN = "runner-token";
// Fixtures live under sdk/ so the generated plugin files can resolve "effect" and the SDK.
const ROOT = path.join(import.meta.dir, "..", `.runner-fixtures-${process.pid}`);
fs.mkdirSync(ROOT, { recursive: true });
afterAll(() => fs.rmSync(ROOT, { recursive: true, force: true }));

const mkdirs = (name: string) => {
	const dir = path.join(ROOT, name);
	fs.mkdirSync(path.join(dir, "scripts"), { recursive: true });
	fs.mkdirSync(path.join(dir, "plugins", "node_modules"), { recursive: true });
	return {
		scriptsDir: path.join(dir, "scripts"),
		pluginsDir: path.join(dir, "plugins"),
		dir,
	};
};

const write = (file: string, content: string) => {
	fs.mkdirSync(path.dirname(file), { recursive: true });
	fs.writeFileSync(file, content);
	fs.chmodSync(file, 0o644);
};

const mockHost = () =>
	Bun.serve({
		port: 0,
		fetch(req) {
			if (req.headers.get("authorization") !== `Bearer ${TOKEN}`)
				return Response.json({ error: "no" }, { status: 401 });
			const p = new URL(req.url).pathname;
			if (p === "/api/v1/host") return Response.json({ hostname: "mock" });
			if (p === "/api/v1/status") return Response.json({ ok: true });
			return Response.json({ error: "nf" }, { status: 404 });
		},
	});

const waitFor = async (predicate: () => boolean, ms = 5000) => {
	const deadline = Date.now() + ms;
	while (!predicate()) {
		if (Date.now() > deadline) throw new Error("condition never became true");
		await new Promise((r) => setTimeout(r, 25));
	}
};

describe("discovery", () => {
	test("finds scripts + plugin packages, refuses world-writable files", () => {
		const d = mkdirs("discover");
		write(path.join(d.scriptsDir, "b-script.ts"), "export {};");
		write(path.join(d.scriptsDir, "a-script.ts"), "export {};");
		write(path.join(d.scriptsDir, "notes.txt"), "not code");
		const evil = path.join(d.scriptsDir, "evil.ts");
		write(evil, "export {};");
		fs.chmodSync(evil, 0o777);
		write(
			path.join(d.pluginsDir, "node_modules", "punktfunk-plugin-x", "package.json"),
			JSON.stringify({ name: "punktfunk-plugin-x", main: "index.js" }),
		);
		write(
			path.join(d.pluginsDir, "node_modules", "punktfunk-plugin-x", "index.js"),
			"export default { name: 'x', main: async () => {} };",
		);
		write(
			path.join(d.pluginsDir, "node_modules", "unrelated-pkg", "package.json"),
			JSON.stringify({ name: "unrelated-pkg" }),
		);

		const logs: string[] = [];
		const units = discoverUnits(d, (l) => logs.push(l));
		expect(units.map((u) => u.name)).toEqual([
			"a-script",
			"b-script",
			"punktfunk-plugin-x",
		]);
		expect(logs.join("\n")).toContain("REFUSING");
		expect(logs.join("\n")).toContain("evil.ts");
	});

	test("discovers scoped @punktfunk/plugin-* packages (not other scoped pkgs)", () => {
		const d = mkdirs("discover-scoped");
		write(
			path.join(d.pluginsDir, "node_modules", "@punktfunk", "plugin-y", "package.json"),
			JSON.stringify({ name: "@punktfunk/plugin-y", module: "dist/index.js" }),
		);
		write(
			path.join(d.pluginsDir, "node_modules", "@punktfunk", "plugin-y", "dist", "index.js"),
			"export default { name: 'y', main: async () => {} };",
		);
		// A non-plugin scoped package (e.g. the SDK itself) is ignored.
		write(
			path.join(d.pluginsDir, "node_modules", "@punktfunk", "host", "package.json"),
			JSON.stringify({ name: "@punktfunk/host" }),
		);
		const units = discoverUnits(d);
		expect(units.map((u) => u.name)).toEqual(["@punktfunk/plugin-y"]);
	});
});

describe("supervision", () => {
	test("async-fn plugin runs with a facade client; clean return completes", async () => {
		const server = mockHost();
		const d = mkdirs("fn-plugin");
		const out = path.join(d.dir, "out.txt");
		write(
			path.join(d.scriptsDir, "fn.ts"),
			`export default { name: "fn", main: async (pf) => {
				const host = await pf.request("GET", "/host");
				require("node:fs").writeFileSync(${JSON.stringify(out)}, host.hostname);
			}};`,
		);
		const logs: string[] = [];
		try {
			const fiber = Effect.runFork(
				runner({
					...d,
					connect: { url: `http://127.0.0.1:${server.port}`, token: TOKEN },
					log: (l) => logs.push(l),
				}),
			);
			await waitFor(() => fs.existsSync(out));
			expect(fs.readFileSync(out, "utf8")).toBe("mock");
			await waitFor(() => logs.some((l) => l.includes("[fn] plugin completed")));
			await Effect.runPromise(Fiber.interrupt(fiber));
		} finally {
			server.stop(true);
		}
	});

	test("a crashing plugin is restarted with backoff", async () => {
		const server = mockHost();
		const d = mkdirs("crashy");
		const counter = path.join(d.dir, "count.txt");
		write(
			path.join(d.scriptsDir, "crashy.ts"),
			`import * as fs from "node:fs";
			export default { name: "crashy", main: async () => {
				const n = fs.existsSync(${JSON.stringify(counter)}) ? Number(fs.readFileSync(${JSON.stringify(counter)}, "utf8")) : 0;
				fs.writeFileSync(${JSON.stringify(counter)}, String(n + 1));
				throw new Error("boom " + n);
			}};`,
		);
		const logs: string[] = [];
		try {
			const fiber = Effect.runFork(
				runner({
					...d,
					connect: { url: `http://127.0.0.1:${server.port}`, token: TOKEN },
					restartBase: "20 millis",
					log: (l) => logs.push(l),
				}),
			);
			await waitFor(() => {
				try {
					return Number(fs.readFileSync(counter, "utf8")) >= 3;
				} catch {
					return false;
				}
			});
			expect(logs.some((l) => l.includes("[crashy] failed: "))).toBe(true);
			expect(logs.some((l) => l.includes("restarting (attempt 2)"))).toBe(true);
			await Effect.runPromise(Fiber.interrupt(fiber));
		} finally {
			server.stop(true);
		}
	});

	test("a bare script is one-shot: runs on import, never restarts", async () => {
		const d = mkdirs("bare");
		const counter = path.join(d.dir, "ran.txt");
		write(
			path.join(d.scriptsDir, "once.ts"),
			`import * as fs from "node:fs";
			const n = fs.existsSync(${JSON.stringify(counter)}) ? Number(fs.readFileSync(${JSON.stringify(counter)}, "utf8")) : 0;
			fs.writeFileSync(${JSON.stringify(counter)}, String(n + 1));`,
		);
		const logs: string[] = [];
		const fiber = Effect.runFork(
			runner({ ...d, restartBase: "20 millis", log: (l) => logs.push(l) }),
		);
		await waitFor(() => logs.some((l) => l.includes("[once] script completed")));
		await new Promise((r) => setTimeout(r, 200)); // would have restarted by now
		expect(fs.readFileSync(counter, "utf8")).toBe("1");
		await Effect.runPromise(Fiber.interrupt(fiber));
	});

	test("shutdown interrupts an Effect plugin STRUCTURALLY — its finalizer runs", async () => {
		const server = mockHost();
		const d = mkdirs("finalizer");
		const acquired = path.join(d.dir, "acquired.txt");
		const released = path.join(d.dir, "released.txt");
		write(
			path.join(d.scriptsDir, "holder.ts"),
			`import { Effect } from "effect";
			import * as fs from "node:fs";
			export default { name: "holder", main: Effect.scoped(Effect.gen(function* () {
				yield* Effect.acquireRelease(
					Effect.sync(() => fs.writeFileSync(${JSON.stringify(acquired)}, "1")),
					() => Effect.sync(() => fs.writeFileSync(${JSON.stringify(released)}, "1")),
				);
				yield* Effect.never; // hold the resource for the plugin's lifetime
			})) };`,
		);
		try {
			const fiber = Effect.runFork(
				runner({
					...d,
					connect: { url: `http://127.0.0.1:${server.port}`, token: TOKEN },
					log: () => {},
				}),
			);
			await waitFor(() => fs.existsSync(acquired));
			expect(fs.existsSync(released)).toBe(false);
			await Effect.runPromise(Fiber.interrupt(fiber)); // the SIGTERM path
			await waitFor(() => fs.existsSync(released));
		} finally {
			server.stop(true);
		}
	});

	test("supervised unit ends cleanly when its plugin completes (no spin)", async () => {
		const server = mockHost();
		const d = mkdirs("done");
		write(
			path.join(d.scriptsDir, "done.ts"),
			`export default { name: "done", main: async () => {} };`,
		);
		const logs: string[] = [];
		try {
			await Effect.runPromise(
				superviseUnit(
					{ name: "done", file: path.join(d.scriptsDir, "done.ts") },
					{
						connect: { url: `http://127.0.0.1:${server.port}`, token: TOKEN },
						log: (l) => logs.push(l),
						restartBase: "10 millis",
					},
				),
			);
			expect(logs.filter((l) => l.includes("restarting")).length).toBe(0);
		} finally {
			server.stop(true);
		}
	});
});
