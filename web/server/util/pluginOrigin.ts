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
