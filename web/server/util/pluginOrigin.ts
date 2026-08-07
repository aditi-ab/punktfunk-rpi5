// Which listener a request arrived on, and where plugin UIs live.
//
// Plugin UIs are served from a DIFFERENT ORIGIN than the console (a second listener on its own
// port — see nitro-entry/bun-https.mjs for why). Two things need to know about that split: the
// gate, which enforces that neither origin serves the other's paths, and the console UI, which has
// to build the iframe URL against the right origin.
import type { H3Event } from "h3";
import { getRequestHeader } from "h3";

/** Set by the server entry on every request; any inbound copy is stripped first. */
const LISTENER_HEADER = "x-pf-listener";

export type Listener = "console" | "plugin";

/**
 * Which listener served this request. Absent ⇒ `console`, which is the safe default: it is what
 * `vite dev` looks like (one listener, and its own middleware intercepts `/plugin-ui` before Nitro
 * ever sees it), and treating an unknown lane as the console means the plugin-path refusal below
 * applies rather than the console-path one — deny the escalation, not the ordinary console.
 */
export function listenerOf(event: H3Event): Listener {
	return getRequestHeader(event, LISTENER_HEADER) === "plugin"
		? "plugin"
		: "console";
}

/** Paths the plugin origin serves. Everything else on that origin is refused. */
export function isPluginUiPath(pathname: string): boolean {
	return pathname === "/plugin-ui" || pathname.startsWith("/plugin-ui/");
}

/**
 * The port the plugin-UI origin is listening on, or `null` when there is none — either the bind
 * failed (production: plugin UIs are disabled, deliberately, rather than falling back to the
 * console origin) or this is `vite dev`, which serves everything from one port.
 *
 * Read from the value the entry SETS after a successful bind, never from the configured-but-unbound
 * one, so this can never advertise a port nothing is listening on.
 */
export function pluginOriginPort(): number | null {
	const raw = process.env.PUNKTFUNK_UI_PLUGIN_PORT_ACTIVE;
	const port = raw ? Number(raw) : Number.NaN;
	return Number.isInteger(port) && port > 0 ? port : null;
}

/** The console's own port, for the plugin origin's `frame-ancestors`. */
export function consoleOriginPort(): number | null {
	const raw = process.env.PUNKTFUNK_UI_CONSOLE_PORT_ACTIVE;
	const port = raw ? Number(raw) : Number.NaN;
	return Number.isInteger(port) && port > 0 ? port : null;
}

/**
 * The scheme the console is actually served on (`https` when TLS is configured), or `null` when the
 * entry has not stamped one — `vite dev`, or a test importing this directly.
 *
 * ⚠ This cannot be read off the request. Nitro's `localFetch` hands the app a SYNTHETIC request with
 * no TLS socket, so `getRequestURL(event).protocol` is `http:` even when the listener is HTTPS. That
 * is harmless for a relative redirect, and was NOT harmless for `frame-ancestors`: the plugin origin
 * named `http://host:47992` as its only permitted ancestor while the console the operator was
 * looking at was `https://host:47992`, and the browser refused to frame the plugin
 * (`ERR_BLOCKED_BY_RESPONSE`) — an empty panel with the explanation only in the devtools console.
 * The scheme-part upgrade in CSP3 (an `http` source also matching an `https` URL) does NOT rescue
 * this: Chromium enforces `frame-ancestors` against the ancestor's origin strictly. Verified on
 * glass, 2026-08-06.
 *
 * Stamped by the entry from the same `tls` option both listeners are built with, so the two can
 * never disagree, and never read from the environment we inherited.
 */
export function consoleOriginScheme(): "http" | "https" | null {
	const raw = process.env.PUNKTFUNK_UI_SCHEME_ACTIVE;
	return raw === "https" || raw === "http" ? raw : null;
}

/**
 * The `frame-ancestors` source naming the console, as a pure rule over the three things that can
 * know the scheme — kept separate from the request so it can be tested, because the bug it exists
 * to prevent is invisible in a header (`http://…` looks perfectly well-formed) and only shows up as
 * a plugin panel that never fills in.
 *
 * Precedence, and why:
 *  1. `x-forwarded-proto` — something in front terminated TLS, so it, not us, knows what the
 *     browser's address bar says. The only case where the two legitimately differ.
 *  2. the scheme the listener was built with — the normal path, stamped at bind time.
 *  3. the request's own scheme — last resort (nothing stamped: `vite dev`, or a direct import).
 */
export function frameAncestorSource(o: {
	forwardedProto?: string | null;
	listenerScheme?: "http" | "https" | null;
	requestScheme: string;
	hostname: string;
	port: number;
}): string {
	const forwarded = o.forwardedProto?.split(",")[0]?.trim().toLowerCase();
	const scheme =
		forwarded === "https" || forwarded === "http"
			? forwarded
			: (o.listenerScheme ?? o.requestScheme.replace(/:$/, ""));
	return `${scheme}://${o.hostname}:${o.port}`;
}
