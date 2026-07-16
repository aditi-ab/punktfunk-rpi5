// Connection resolution (RFC §7): loopback URL + bearer token + the host's self-signed
// identity cert, from the environment with file fallbacks — so `connect()` on the host machine
// needs zero configuration.
//
//   PUNKTFUNK_MGMT_URL    (default https://127.0.0.1:47990)
//   PUNKTFUNK_MGMT_TOKEN  (else <config_dir>/mgmt-token)
//   PUNKTFUNK_MGMT_CA     (path; else <config_dir>/cert.pem when present)
//
// The CA is the host's own identity certificate — trusting exactly it (not the system roots)
// IS the pin for the loopback hop. Per-runtime plumbing differs: Bun takes `tls.ca` on fetch,
// Node (undici) takes a dispatcher with a CA-carrying TLS connector; anything else falls back
// to plain fetch (document PUNKTFUNK_MGMT_CA + NODE_EXTRA_CA_CERTS there).
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

export interface ConnectOptions {
	/** Management API base URL (default `https://127.0.0.1:47990`). */
	url?: string;
	/** Bearer token (default: `PUNKTFUNK_MGMT_TOKEN`, else the host's `mgmt-token` file). */
	token?: string;
	/** PEM of the CA to trust — the host's identity cert (default: `PUNKTFUNK_MGMT_CA`, else `cert.pem`). */
	ca?: string;
}

export interface ResolvedConfig {
	url: string;
	token: string;
	ca?: string;
	/** A fetch honoring `ca` on this runtime. */
	fetch: typeof fetch;
}

/** The host's config dir — the same resolution the host itself uses. */
export const configDir = (): string => {
	const explicit = process.env.PUNKTFUNK_CONFIG_DIR;
	if (explicit) return explicit;
	if (process.platform === "win32") {
		const base = process.env.ProgramData ?? process.env.APPDATA ?? ".";
		return path.join(base, "punktfunk");
	}
	const base =
		process.env.XDG_CONFIG_HOME ?? path.join(os.homedir(), ".config");
	return path.join(base, "punktfunk");
};

const readIfExists = (p: string): string | undefined => {
	try {
		return fs.readFileSync(p, "utf8");
	} catch {
		return undefined;
	}
};

/** First token-looking line of the mgmt-token file (tolerates `TOKEN=`-style and blank lines). */
const parseTokenFile = (raw: string): string | undefined => {
	for (const line of raw.split(/\r?\n/)) {
		const t = line.trim();
		if (t.length === 0 || t.startsWith("#")) continue;
		return t.includes("=") ? t.slice(t.indexOf("=") + 1).trim() : t;
	}
	return undefined;
};

export const resolveConfig = async (
	options?: ConnectOptions,
): Promise<ResolvedConfig> => {
	const url = (
		options?.url ??
		process.env.PUNKTFUNK_MGMT_URL ??
		"https://127.0.0.1:47990"
	).replace(/\/+$/, "");
	const token =
		options?.token ??
		process.env.PUNKTFUNK_MGMT_TOKEN ??
		parseTokenFile(readIfExists(path.join(configDir(), "mgmt-token")) ?? "");
	if (!token) {
		throw new Error(
			"no management token: set PUNKTFUNK_MGMT_TOKEN, pass { token }, or run where " +
				`the host's token file exists (${path.join(configDir(), "mgmt-token")})`,
		);
	}
	const caPath = process.env.PUNKTFUNK_MGMT_CA;
	const ca =
		options?.ca ??
		(caPath ? readIfExists(caPath) : undefined) ??
		(url.startsWith("https://")
			? readIfExists(path.join(configDir(), "cert.pem"))
			: undefined);
	return { url, token, ca, fetch: await makeFetch(ca) };
};

/**
 * A fetch that PINS `ca` — the host's self-signed identity cert — on this runtime.
 *
 * The pin is chain verification against exactly that certificate (nothing else can pass),
 * with the HOSTNAME check waived: the host identity cert is deliberately CN-only/no-SAN
 * (native clients pin its fingerprint; see `web/nitro-entry/bun-https.mjs` for the same
 * finding), so standard SAN matching would always fail — and it adds nothing when the chain
 * already admits only the one pinned cert.
 */
const makeFetch = async (ca: string | undefined): Promise<typeof fetch> => {
	if (!ca) return fetch;
	const skipHostname = { checkServerIdentity: () => undefined };
	// Bun: fetch takes node-compatible `tls` options.
	if (typeof (globalThis as Record<string, unknown>).Bun !== "undefined") {
		return ((input: Parameters<typeof fetch>[0], init?: RequestInit) =>
			fetch(input, {
				...init,
				tls: { ca, ...skipHostname },
			} as RequestInit)) as typeof fetch;
	}
	// Node: global fetch is undici — a per-request dispatcher carries the pin.
	try {
		// Optional dependency — declared in package.json optionalDependencies; absent on
		// runtimes that don't need it (the catch below falls back).
		const { Agent } = (await import("undici" as string)) as {
			Agent: new (opts: unknown) => unknown;
		};
		const dispatcher = new Agent({ connect: { ca, ...skipHostname } });
		return ((input: Parameters<typeof fetch>[0], init?: RequestInit) =>
			fetch(input, { ...init, dispatcher } as RequestInit)) as typeof fetch;
	} catch {
		// Unknown runtime: plain fetch (system trust) — PUNKTFUNK_MGMT_CA via the runtime's
		// own CA mechanism (e.g. NODE_EXTRA_CA_CERTS / --cert) is the documented fallback.
		return fetch;
	}
};
