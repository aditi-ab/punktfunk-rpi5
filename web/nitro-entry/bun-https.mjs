// Custom Nitro server entry for the punktfunk web console.
//
// It is the stock Nitro `bun` preset entry
// (node_modules/nitropack/dist/presets/bun/runtime/bun.mjs) plus **TLS**, so the console is served
// over **HTTPS (HTTP/1.1 over TLS)** using the HOST's own identity cert (the cert native clients
// already pin). One trust anchor across the data plane, the management API, and this console. Wired
// in via `entry:` in vite.config.ts on top of Nitro's `bun` preset (which bundles the handler in).
//
// NOTE on HTTP/2 + HTTP/3: NOT offered here, on purpose. `Bun.serve` has no HTTP/2 server, and
// HTTP/3 (which Bun *can* do) is useless to a browser against this cert: QUIC refuses any cert error,
// and the host identity cert is a CN-only, no-SAN, self-signed cert (correct for native fingerprint
// PINNING, rejected by browsers). So browsers stay on HTTP/1.1 regardless — advertising h3 would just
// dangle an `Alt-Svc` no browser can use. Real h2/h3 would need a browser-TRUSTED, SAN-matching cert
// (a local CA installed per device) fronted by a server that speaks them (e.g. Caddy) — deliberately
// out of scope for a LAN console; TLS (no cleartext login/session) is the win.
//
// Env (set by the launchers / the systemd unit — see web.env.example):
//   PUNKTFUNK_UI_TLS_CERT / _KEY   PEM file paths (the host's cert.pem / key.pem). BOTH set ⇒ HTTPS.
//                                  Unset ⇒ plain HTTP (local dev only).
//   PORT / HOST                    standard Nitro bind (3000 / 0.0.0.0).
import "#nitro-internal-pollyfills";
import wsAdapter from "crossws/adapters/bun";
import { useNitroApp } from "nitropack/runtime";
import { startScheduleRunner } from "nitropack/runtime/internal";

const nitroApp = useNitroApp();
const ws = import.meta._websocket
	? wsAdapter(nitroApp.h3App.websocket)
	: undefined;

// The socket peer, handed to the app as a trusted header.
//
// Nitro's `localFetch` (below) hands the app a SYNTHETIC request whose socket has no
// `remoteAddress`, so h3's `getRequestIP()` returns undefined *inside* the app and every
// per-peer decision collapses onto one shared bucket. That silently defeated the login
// throttle: five wrong passwords from anywhere locked out everyone, including the operator
// (and, since the update-apply route shares that budget, locked out host updates too).
// `server.requestIP(req)` is the only place the real peer is knowable, so we stamp it here.
// Any inbound copy is deleted first, so a client cannot forge it.
// Read back by `peerAddress()` in server/util/auth.ts — keep the two names in sync.
const PEER_IP_HEADER = "x-pf-peer-ip";

// TLS from the host's identity cert (file PATHS → Bun.file, not PEM-in-env). Absent ⇒ plain HTTP.
const certPath = process.env.PUNKTFUNK_UI_TLS_CERT;
const keyPath = process.env.PUNKTFUNK_UI_TLS_KEY;
const tls =
	certPath && keyPath
		? { cert: Bun.file(certPath), key: Bun.file(keyPath) }
		: undefined;

const server = Bun.serve({
	port: process.env.NITRO_PORT || process.env.PORT || 3000,
	host: process.env.NITRO_HOST || process.env.HOST,
	// Bun defaults this to 10 s, which is SHORTER than the host's 15 s SSE keep-alive comment — so a
	// proxied `/api/v1/events` stream (or any other quiet long-lived response) gets cut by us and
	// reconnects on a loop. 120 s is comfortably above any keep-alive we forward; still overridable.
	idleTimeout:
		Number.parseInt(process.env.NITRO_BUN_IDLE_TIMEOUT, 10) || 120,
	// `tls: undefined` ⇒ plain HTTP (dev); otherwise HTTPS over HTTP/1.1.
	tls,
	websocket: import.meta._websocket ? ws.websocket : undefined,
	async fetch(req, server) {
		if (import.meta._websocket && req.headers.get("upgrade") === "websocket") {
			return ws.handleUpgrade(req, server);
		}
		const url = new URL(req.url);
		let body;
		if (req.body) {
			body = await req.arrayBuffer();
		}
		// Strip any client-supplied value BEFORE stamping the real one (see PEER_IP_HEADER).
		const headers = new Headers(req.headers);
		headers.delete(PEER_IP_HEADER);
		const peer = server.requestIP(req)?.address;
		if (peer) headers.set(PEER_IP_HEADER, peer);
		return nitroApp.localFetch(url.pathname + url.search, {
			host: url.hostname,
			protocol: url.protocol,
			headers,
			method: req.method,
			redirect: req.redirect,
			body,
		});
	},
});
console.log(`punktfunk web console listening on ${server.url} (tls=${!!tls})`);
if (import.meta._tasks) {
	startScheduleRunner();
}
