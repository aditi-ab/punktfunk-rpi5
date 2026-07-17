// The typed `pf.api.*` surface end to end against a mock host: proves the generated client is
// reachable through the facade — base URL, bearer auth, the CA-pinning fetch injection, and
// schema decode all wired. (The event stream is covered by surfaces.test.ts.)
import { describe, expect, test } from "bun:test";
import { connect } from "../src/index.js";

const TOKEN = "test-token";

const mockHost = () =>
	Bun.serve({
		port: 0,
		fetch(req) {
			const url = new URL(req.url);
			if (req.headers.get("authorization") !== `Bearer ${TOKEN}`) {
				return Response.json({ error: "invalid credentials" }, { status: 401 });
			}
			switch (url.pathname) {
				case "/api/v1/host":
					return Response.json({ hostname: "mock" }); // connect() probe (undecoded)
				case "/api/v1/clients":
					return Response.json([
						{ fingerprint: "abc123", subject: "CN=Living Room TV" },
					]);
				case "/api/v1/session":
					// stopSession → 204 No Content (idempotent).
					return new Response(null, { status: 204 });
				default:
					return Response.json({ error: "not found" }, { status: 404 });
			}
		},
	});

describe("typed api surface", () => {
	test("pf.api.* returns decoded, typed results", async () => {
		const server = mockHost();
		try {
			const pf = await connect({
				url: `http://127.0.0.1:${server.port}`,
				token: TOKEN,
			});

			const clients = await pf.api.listPairedClients();
			expect(clients).toHaveLength(1);
			// Typed field access — no cast.
			expect(clients[0]?.fingerprint).toBe("abc123");
			expect(clients[0]?.subject).toBe("CN=Living Room TV");

			// A 204 endpoint resolves to void, not a thrown "empty body".
			await expect(pf.api.stopSession()).resolves.toBeUndefined();

			pf.close();
		} finally {
			server.stop(true);
		}
	});

	test("a failing call rejects", async () => {
		const server = mockHost();
		try {
			const pf = await connect({
				url: `http://127.0.0.1:${server.port}`,
				token: TOKEN,
			});
			// /api/v1/gpus isn't served → 404 → the endpoint's typed error rejects.
			await expect(pf.api.listGpus()).rejects.toThrow();
			pf.close();
		} finally {
			server.stop(true);
		}
	});
});
