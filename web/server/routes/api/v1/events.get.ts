// GET /api/v1/events — the host's SSE lifecycle stream, proxied with the body left STREAMING.
//
// Why this route exists at all, when the `/api/**` catch-all already proxies everything:
// the generic path cannot stream. h3's `proxyRequest` pumps the upstream body into the node-style
// response with `res.write`, and under the deployed Bun entry that response is a `node-mock-http`
// object whose writes are accumulated and only turned into a real Response when the handler
// returns. Measured: three frames sent one second apart arrive at the browser together, ~3 s late,
// when the upstream closes. For an SSE stream that is fatal — it never closes, so nothing ever
// arrives, and every event-driven update in the console would silently never fire.
//
// Returning a WEB `Response` whose body is the upstream's own `ReadableStream` sidesteps the
// node-response emulation entirely: h3 hands it back as-is and the Bun entry passes it through.
//
// Everything else matches the catch-all: session-gated by middleware/auth.ts, mgmt bearer injected
// server-side, TLS relaxed only for the loopback hop, 401 → 502.
import {
	createError,
	defineEventHandler,
	getRequestHeader,
	getRequestURL,
} from "h3";
import { isLoopbackUrl, mgmtToken, mgmtUrl } from "../../../util/auth";

export default defineEventHandler(async (event) => {
	const token = mgmtToken();
	if (!token) {
		throw createError({
			statusCode: 503,
			statusMessage: "management token not configured",
		});
	}
	const base = mgmtUrl();
	const { search } = getRequestURL(event);
	const headers: Record<string, string> = {
		authorization: `Bearer ${token}`,
		accept: "text/event-stream",
		// Ask for no compression: a buffering encoder defeats the point of a live stream.
		"accept-encoding": "identity",
	};
	// Forward the SSE resume cursor so a reconnect replays from the host's ring rather than
	// silently skipping whatever happened while we were away.
	const lastId = getRequestHeader(event, "last-event-id");
	if (lastId) headers["last-event-id"] = lastId;

	const init: RequestInit = { method: "GET", headers, redirect: "manual" };
	if (isLoopbackUrl(base)) {
		// Bun.fetch extension — scoped per request, never process-wide (see routes/api/[...].ts).
		(init as unknown as { tls: { rejectUnauthorized: boolean } }).tls = {
			rejectUnauthorized: false,
		};
	}

	let upstream: Response;
	try {
		upstream = await fetch(`${base}/api/v1/events${search}`, init);
	} catch (cause) {
		throw createError({
			statusCode: 502,
			statusMessage: "management API unreachable",
			cause,
		});
	}
	if (upstream.status === 401) {
		throw createError({
			statusCode: 502,
			statusMessage:
				"management API rejected the host token (check PUNKTFUNK_MGMT_TOKEN)",
		});
	}
	if (!upstream.ok || !upstream.body) {
		throw createError({
			statusCode: 502,
			statusMessage: `management API refused the event stream (${upstream.status})`,
		});
	}

	// The upstream body, untouched. `no-transform` + `X-Accel-Buffering: no` tell any intermediary
	// (and Nitro's own compression) to keep their hands off a live stream.
	return new Response(upstream.body, {
		status: 200,
		headers: {
			"content-type": "text/event-stream; charset=utf-8",
			"cache-control": "no-cache, no-transform",
			connection: "keep-alive",
			"x-accel-buffering": "no",
		},
	});
});
