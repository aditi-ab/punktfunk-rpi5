// GET /_auth/ui-config — the handful of deployment facts the console UI cannot work out for itself.
//
// Today that is exactly one: where plugin UIs live. They are served from a different ORIGIN than
// the console (2026-08-05 review H-3), so the browser needs the port to build the iframe URL — and
// it must come from the server, because only the server knows whether that listener actually bound.
//
// Public (the `/_auth/` prefix is), which is fine: a port number is discoverable by connecting to
// it, and nothing here is a secret. Deliberately NOT an inference the client makes for itself
// (`location.port + 1` would silently point at whatever else is on that port).
import { defineEventHandler } from "h3";
import { type OmarchyTheme, omarchyTheme } from "../../util/omarchyTheme";
import { pluginOriginPort } from "../../util/pluginOrigin";

export interface UiConfig {
	/**
	 * How plugin UIs are reachable:
	 *  - `origin`      — from their own origin on `pluginPort` (the deployed, secure arrangement)
	 *  - `same-origin` — `vite dev` only: one listener, and its own middleware serves `/plugin-ui`
	 *  - `unavailable` — the plugin listener could not bind. Plugin UIs are OFF; the console must
	 *                    not fall back to its own origin, which is the hole this all exists to close.
	 */
	pluginUi: "origin" | "same-origin" | "unavailable";
	pluginPort: number | null;
	/**
	 * The active Omarchy theme, when this box has one — `null` everywhere else, which is every
	 * non-Omarchy box and every Omarchy box whose operator did not opt in. The console keys its
	 * own palette off it so the page matches the desktop that launched it.
	 */
	theme: OmarchyTheme | null;
}

export default defineEventHandler((): UiConfig => {
	// Read per request: `omarchy-theme-set` rewrites the file whenever the user switches theme,
	// and the client refetches on navigation, so the console follows without a restart.
	const theme = omarchyTheme();
	const port = pluginOriginPort();
	if (port) return { pluginUi: "origin", pluginPort: port, theme };
	// `import.meta.dev` is Nitro's build-time dev flag — false in every shipped build, so a
	// production bind failure can never resolve to the same-origin arrangement.
	if (import.meta.dev)
		return { pluginUi: "same-origin", pluginPort: null, theme };
	return { pluginUi: "unavailable", pluginPort: null, theme };
});
