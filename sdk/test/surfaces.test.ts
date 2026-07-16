// Both surfaces against one mock host: the Promise facade end to end, and the Effect surface's
// request/get/events with typed errors.
import { describe, expect, test } from "bun:test";
import { Effect, Schema as S, Stream } from "effect";
import { connect } from "../src/index.js";
import * as pf from "../src/effect.js";

const TOKEN = "test-token";

/** A minimal mock host: auth-checked /host, /status, and an SSE /events feed. */
const mockHost = () => {
	const emitted = new TextEncoder().encode(
		'id: 7\nevent: pairing.pending\ndata: {"seq":7,"ts_ms":3,"schema":1,"kind":"pairing.pending","device":{"name":"iPad Pro","fingerprint":"ab12","plane":"native"}}\n\n' +
			'id: 8\nevent: future.kind\ndata: {"seq":8,"ts_ms":4,"schema":1,"kind":"future.kind"}\n\n' +
			'id: 9\nevent: library.changed\ndata: {"seq":9,"ts_ms":5,"schema":1,"kind":"library.changed","source":"manual"}\n\n',
	);
	return Bun.serve({
		port: 0,
		fetch(req) {
			const url = new URL(req.url);
			if (req.headers.get("authorization") !== `Bearer ${TOKEN}`) {
				return Response.json({ error: "missing or invalid credentials" }, { status: 401 });
			}
			switch (url.pathname) {
				case "/api/v1/host":
					return Response.json({ name: "mock", version: "0.12.0" });
				case "/api/v1/status":
					return Response.json({ ok: true, sessions: 0 });
				case "/api/v1/boom":
					return Response.json({ error: "kaput" }, { status: 500 });
				case "/api/v1/events": {
					const body = new ReadableStream({
						start(c) {
							c.enqueue(emitted); // then stay open
						},
					});
					return new Response(body, { headers: { "content-type": "text/event-stream" } });
				}
				default:
					return Response.json({ error: "not found" }, { status: 404 });
			}
		},
	});
};

describe("promise facade", () => {
	test("connect → request → typed events → unknown channel → close", async () => {
		const server = mockHost();
		try {
			const client = await connect({ url: `http://127.0.0.1:${server.port}`, token: TOKEN });
			const status = (await client.request("GET", "/status")) as { ok: boolean };
			expect(status.ok).toBe(true);

			const got: string[] = [];
			const unknown: unknown[] = [];
			const done = new Promise<void>((resolve) => {
				client.events.on("pairing.pending", (e) => {
					got.push(`pending:${e.device.name}`);
				});
				client.events.on("library.*", (e) => {
					got.push(`lib:${e.kind}`);
					resolve();
				});
				client.events.on("unknown", (e) => unknown.push(e));
			});
			await done;
			expect(got).toEqual(["pending:iPad Pro", "lib:library.changed"]);
			expect(unknown.length).toBe(1);
			client.close();
		} finally {
			server.stop(true);
		}
	});

	test("connect fails fast on a bad token", async () => {
		const server = mockHost();
		try {
			await expect(
				connect({ url: `http://127.0.0.1:${server.port}`, token: "wrong" }),
			).rejects.toThrow(/credentials/);
		} finally {
			server.stop(true);
		}
	});
});

describe("effect surface", () => {
	test("request + schema get + typed errors + event stream", async () => {
		const server = mockHost();
		const live = pf.PunktfunkHostLive({ url: `http://127.0.0.1:${server.port}`, token: TOKEN });
		try {
			const program = Effect.gen(function* () {
				const status = yield* pf.get("/status", S.Struct({ ok: S.Boolean }));
				expect(status.ok).toBe(true);

				// A wrong shape is VersionSkew, not undefined-later.
				const skew = yield* pf
					.get("/status", S.Struct({ nope: S.String }))
					.pipe(Effect.flip);
				expect(skew._tag).toBe("VersionSkew");

				// A host error carries its ApiError envelope message.
				const boom = yield* pf.request("GET", "/boom").pipe(Effect.flip);
				expect(boom._tag).toBe("ApiError");
				if (boom._tag === "ApiError") expect(boom.message).toBe("kaput");

				// The decoded stream skips the unknown kind and delivers the known ones.
				const events = yield* pf.events().pipe(Stream.take(2), Stream.runCollect);
				const kinds = [...events].map((e) => e.kind);
				expect(kinds).toEqual(["pairing.pending", "library.changed"]);
			});
			await Effect.runPromise(program.pipe(Effect.provide(live)));
		} finally {
			server.stop(true);
		}
	});
});
