// Where plugin UIs live, from the server that knows.
//
// Plugin UIs are served from a DIFFERENT ORIGIN than the console (2026-08-05 review H-3): same
// scheme and host, its own port. The console has to build iframe and new-tab URLs against that
// origin, and the port has to come from the server — only it knows whether the listener bound.
import { useQuery } from "@tanstack/react-query";

export interface UiConfig {
	pluginUi: "origin" | "same-origin" | "unavailable";
	pluginPort: number | null;
}

/**
 * Deployment facts the console cannot infer. Cached for the session — the ports cannot change
 * without the server restarting, which reloads the page anyway.
 */
export const useUiConfig = () =>
	useQuery({
		queryKey: ["ui-config"],
		queryFn: async (): Promise<UiConfig> => {
			const r = await fetch("/_auth/ui-config", {
				credentials: "same-origin",
			});
			if (!r.ok) throw new Error(`ui-config ${r.status}`);
			return (await r.json()) as UiConfig;
		},
		staleTime: Number.POSITIVE_INFINITY,
		retry: 2,
	});

/**
 * The origin serving plugin UIs, or `null` when there is none and the console must say so rather
 * than render a frame.
 *
 * Built from the CURRENT location's scheme and hostname, so it follows whatever address the
 * operator actually browsed to — an IP, an mDNS name, a hostname — and only the port differs. That
 * matters for more than cosmetics: it keeps the origin same-SITE with the console, which is what
 * lets the `SameSite=Lax` session cookie reach the plugin listener at all.
 */
export function pluginOriginFrom(
	config: UiConfig | undefined,
): string | null | undefined {
	if (!config) return undefined; // still loading — render neither frame nor error
	if (config.pluginUi === "same-origin") return ""; // vite dev: relative URLs, one origin
	if (config.pluginUi === "origin" && config.pluginPort) {
		return `${window.location.protocol}//${window.location.hostname}:${config.pluginPort}`;
	}
	return null; // unavailable — the listener did not bind
}
