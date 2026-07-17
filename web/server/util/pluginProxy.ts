// Server-side helper for the plugin-UI reverse proxy (plugin-ui-surface §5). The console proxies
// `/plugin-ui/<id>/**` to a plugin's loopback UI server, injecting the plugin's per-boot secret —
// which it fetches here, from the management API, **server-side only** (the secret never reaches the
// browser; the BFF additionally denylists the credential endpoint from the generic passthrough).
//
// The credential is cached briefly so a burst of iframe asset requests doesn't hammer the host. On a
// 401 from the plugin (its secret rotated on restart within the cache window) the proxy busts this
// cache and re-fetches once — see the route.
import { isLoopbackUrl, mgmtToken, mgmtUrl } from "./auth";

/** A plugin id — its `definePlugin` name; the same shape the host validates. */
export const PLUGIN_ID_RE = /^[a-z][a-z0-9-]*$/;

/** The proxy credential for a plugin's loopback UI. */
export interface UiCredential {
	port: number;
	secret: string;
}

const TTL_MS = 15_000;
const cache = new Map<string, { cred: UiCredential | null; at: number }>();

/** Drop a cached credential (called when a plugin's secret proved stale). */
export function bustCredential(id: string): void {
	cache.delete(id);
}

/**
 * Fetch `{port, secret}` for a plugin's UI from the management API (bearer, loopback). Returns
 * `null` when the plugin isn't registered / has no UI (a 404). Results are cached for {@link TTL_MS};
 * pass `bustCache` to force a fresh read (the stale-secret retry). Throws only on a missing mgmt
 * token (a deploy misconfig) — a transient upstream error resolves to `null` (treated as offline)
 * and is not cached.
 */
export async function fetchUiCredential(
	id: string,
	opts?: { bustCache?: boolean },
): Promise<UiCredential | null> {
	const now = Date.now();
	if (!opts?.bustCache) {
		const hit = cache.get(id);
		if (hit && now - hit.at < TTL_MS) return hit.cred;
	}

	const base = mgmtUrl();
	const token = mgmtToken();
	if (!token) {
		throw new Error(
			"management token not configured (PUNKTFUNK_MGMT_TOKEN / ~/.config/punktfunk/mgmt-token)",
		);
	}
	// The host serves the credential over HTTPS with its self-signed loopback cert; relax
	// verification for that one loopback hop only (the same scoping the /api BFF uses).
	const fetchOptions = isLoopbackUrl(base)
		? ({ tls: { rejectUnauthorized: false } } as unknown as RequestInit)
		: undefined;
	const resp = await fetch(`${base}/api/v1/plugins/${id}/ui-credential`, {
		...fetchOptions,
		headers: { authorization: `Bearer ${token}` },
	});

	if (resp.ok) {
		const cred = (await resp.json()) as UiCredential;
		cache.set(id, { cred, at: now });
		return cred;
	}
	if (resp.status === 404) {
		// Definitively not running / no UI — cache the negative so a dead iframe doesn't spin.
		cache.set(id, { cred: null, at: now });
		return null;
	}
	// Transient (401/5xx): don't cache, let the next request retry.
	return null;
}
