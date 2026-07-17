// `servePluginUi` end to end: it registers with the host (secret and all), serves static + dynamic
// + SPA-fallback behind the per-boot secret, renews the lease, and deregisters on close. The mock
// host records what the helper PUTs/DELETEs so we can assert the registration shape.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { connect } from "../src/index.js";
import { servePluginUi } from "../src/ui.js";

const TOKEN = "test-token";

interface Registration {
	id: string;
	body: { title: string; version?: string; ui?: { port: number; secret: string; icon?: string } };
}

// A mock management host that captures plugin registry writes.
const registrations: Registration[] = [];
const deletes: string[] = [];

const host = Bun.serve({
	port: 0,
	async fetch(req) {
		const url = new URL(req.url);
		if (req.headers.get("authorization") !== `Bearer ${TOKEN}`) {
			return Response.json({ error: "invalid credentials" }, { status: 401 });
		}
		if (url.pathname === "/api/v1/host") return Response.json({ hostname: "mock" });
		const m = url.pathname.match(/^\/api\/v1\/plugins\/([a-z][a-z0-9-]*)$/);
		if (m) {
			if (req.method === "PUT") {
				registrations.push({ id: m[1], body: await req.json() });
				return new Response(null, { status: 204 });
			}
			if (req.method === "DELETE") {
				deletes.push(m[1]);
				return new Response(null, { status: 204 });
			}
		}
		return Response.json({ error: "not found" }, { status: 404 });
	},
});

const hostUrl = `http://127.0.0.1:${host.port}`;

// A built SPA on disk: an index and one asset.
let staticDir: string;
beforeAll(() => {
	staticDir = fs.mkdtempSync(path.join(os.tmpdir(), "pf-ui-"));
	fs.writeFileSync(path.join(staticDir, "index.html"), "INDEX");
	fs.writeFileSync(path.join(staticDir, "app.js"), "ASSET");
});
afterAll(() => {
	host.stop(true);
	fs.rmSync(staticDir, { recursive: true, force: true });
});

/** GET the plugin's own server with the captured secret. */
const authed = (base: string, p: string, secret: string, init?: RequestInit) =>
	fetch(`${base}${p}`, { ...init, headers: { authorization: `Bearer ${secret}`, ...init?.headers } });

describe("servePluginUi", () => {
	test("registers, serves, renews, and deregisters", async () => {
		const pf = await connect({ url: hostUrl, token: TOKEN });
		const ui = await servePluginUi(pf, {
			id: "demo",
			title: "Demo",
			version: "9.9.9",
			icon: "puzzle",
			staticDir,
			fetch: (req) =>
				new URL(req.url).pathname === "/api/ping"
					? Response.json({ pong: true })
					: undefined,
			renewIntervalMs: 15,
		});

		// --- registration shape (the secret rides along; the port matches the bound server) ---
		const reg = registrations.find((r) => r.id === "demo");
		expect(reg).toBeDefined();
		expect(reg?.body.title).toBe("Demo");
		expect(reg?.body.version).toBe("9.9.9");
		expect(reg?.body.ui?.port).toBe(ui.port);
		expect(reg?.body.ui?.icon).toBe("puzzle");
		const secret = reg?.body.ui?.secret ?? "";
		expect(secret).toMatch(/^[A-Za-z0-9_-]{43}$/); // 32 random bytes, base64url

		// --- the secret is mandatory on every request ---
		expect((await fetch(`${ui.url}/__health`)).status).toBe(401);
		const health = await authed(ui.url, "/__health", secret);
		expect(health.status).toBe(200);
		expect(await health.json()).toEqual({ ok: true, id: "demo", title: "Demo" });

		// --- static assets, then SPA fallback for a navigation that matched no file ---
		expect(await (await authed(ui.url, "/", secret, { headers: { accept: "text/html" } })).text()).toBe("INDEX");
		expect(await (await authed(ui.url, "/app.js", secret)).text()).toBe("ASSET");
		const spa = await authed(ui.url, "/some/client/route", secret, { headers: { accept: "text/html" } });
		expect(await spa.text()).toBe("INDEX"); // SPA fallback
		// A missing NON-navigation asset is a real 404, not the index.
		expect((await authed(ui.url, "/missing.js", secret)).status).toBe(404);

		// --- the plugin's dynamic handler wins for its own routes ---
		expect(await (await authed(ui.url, "/api/ping", secret)).json()).toEqual({ pong: true });

		// --- lease renewal keeps PUTting on the interval ---
		const before = registrations.filter((r) => r.id === "demo").length;
		await new Promise((r) => setTimeout(r, 60));
		expect(registrations.filter((r) => r.id === "demo").length).toBeGreaterThan(before);

		// --- close deregisters and stops the server ---
		await ui.close();
		expect(deletes).toContain("demo");
		await expect(fetch(`${ui.url}/__health`)).rejects.toThrow(); // connection refused
		pf.close();
	});

	test("rejects a non-kebab id before binding anything", async () => {
		const pf = await connect({ url: hostUrl, token: TOKEN });
		await expect(servePluginUi(pf, { id: "Bad_Id", title: "x" })).rejects.toThrow(/kebab/);
		pf.close();
	});
});
