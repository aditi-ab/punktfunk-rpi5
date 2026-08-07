/**
 * A revocation marker for issued sessions, PERSISTED across restarts.
 *
 * The session is stateless: everything lives inside the sealed cookie, so `session.clear()` only
 * deletes the BROWSER's copy. A cookie captured beforehand stayed valid for its full 7-day TTL —
 * "log out" did not log anything out.
 *
 * The counter has to survive a restart or it does not do its job: an in-memory `let epoch = 1`
 * revokes within one process run, then resets to 1 the next time the service starts, and a cookie
 * captured from that first run is accepted again for the rest of its TTL. (The seal key cannot save
 * us — it is derived from the stable mgmt token, so pre-restart cookies still unseal fine.) So it
 * lives in a file next to the host's own config.
 *
 * Best-effort by design: if the file cannot be read or written the console still works, it just
 * falls back to in-memory revocation for this process. Refusing to log anyone out because a state
 * file is unwritable would be the wrong trade for a LAN console.
 */
const EPOCH_FILE = (): string =>
	process.env.PUNKTFUNK_UI_EPOCH_FILE ??
	join(
		process.env.PUNKTFUNK_CONFIG_DIR ?? join(homedir(), ".config", "punktfunk"),
		"web-session-epoch",
	);

let epochCache: number | null = null;

/** The epoch a new session is stamped with, and the one the gate requires. */
export function sessionEpoch(): number {
	if (epochCache !== null) return epochCache;
	try {
		const raw = readFileSync(EPOCH_FILE(), "utf8").trim();
		const n = Number.parseInt(raw, 10);
		epochCache = Number.isFinite(n) && n > 0 ? n : 1;
	} catch {
		epochCache = 1; // no file yet — first run
	}
	return epochCache;
}

/** Invalidate every session issued so far (what logging out does). */
export function revokeAllSessions(): void {
	const next = sessionEpoch() + 1;
	epochCache = next;
	try {
		mkdirSync(dirname(EPOCH_FILE()), { recursive: true });
		writeFileSync(EPOCH_FILE(), String(next), { mode: 0o600 });
	} catch {
		// Unwritable state dir: the bump still holds for this process, which is the common case
		// (log out, walk away). It is weaker than persisted, and better than refusing to log out.
	}
}

// Shared auth helpers for the Nitro server (the deployed Bun server). Single-user,
// shared-password gate: the user logs in with PUNKTFUNK_UI_PASSWORD, which sets a SEALED
// (h3 useSession — AES-GCM) cookie; every request is gated by server/middleware/auth.ts.
//
// The management token never reaches the browser: server/routes/api/[...].ts injects it
// server-side when proxying to the loopback management API.
import {
	createHash,
	timingSafeEqual as nodeTimingSafeEqual,
} from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import {
	getRequestHeader,
	getRequestIP,
	type H3Event,
	type SessionConfig,
} from "h3";

export const SESSION_NAME = "pf_session";

/** Set by the Bun entry (nitro-entry/bun-https.mjs) to the real socket peer, after deleting any
 * inbound copy. Keep the name in sync with that file. */
const PEER_IP_HEADER = "x-pf-peer-ip";

/**
 * The requesting peer, as the key for every per-peer budget (currently the login throttle).
 *
 * `getRequestIP()` alone does NOT work under the deployed server: Nitro's `localFetch` builds a
 * synthetic request whose socket carries no `remoteAddress`, so h3 finds nothing and every caller
 * collapses onto one shared bucket — which turned the "per-IP" login throttle into a lockout any
 * LAN peer could trigger for everyone. The Bun entry stamps the real peer into PEER_IP_HEADER
 * (unforgeable: it deletes any client-supplied copy first), so prefer that.
 *
 * `getRequestIP` is kept as the fallback for any other ingress (a plain `node`/dev run), and
 * "unknown" as the last resort — a SHARED bucket, deliberately: an unattributable request must
 * still be rate-limited, and failing open would make brute force unbounded.
 */
export function peerAddress(event: H3Event): string {
	const stamped = getRequestHeader(event, PEER_IP_HEADER)?.trim();
	if (stamped) return stamped;
	return getRequestIP(event) ?? "unknown";
}

/** The login password. Empty string ⇒ auth is MISCONFIGURED (the gate fails closed). */
export function uiPassword(): string {
	return process.env.PUNKTFUNK_UI_PASSWORD ?? "";
}

/** The management API the proxy forwards to (loopback by default — never LAN-exposed). It serves
 * HTTPS with the host's self-signed identity cert, so the proxy relaxes verification for that ONE
 * loopback hop via Bun's per-request `tls` option (routes/api/[...].ts, util/forward.ts). There is
 * deliberately no process-wide NODE_TLS_REJECT_UNAUTHORIZED — see .env.example. */
export function mgmtUrl(): string {
	return process.env.PUNKTFUNK_MGMT_URL ?? "https://127.0.0.1:47990";
}

/** Bearer token for the management API, injected server-side. */
export function mgmtToken(): string {
	return process.env.PUNKTFUNK_MGMT_TOKEN ?? "";
}

/** Whether `url`'s host is a loopback address — the only place the proxy relaxes TLS verification
 * for the host's self-signed cert. IPv4 127.0.0.0/8, IPv6 ::1, and the `localhost` name. */
export function isLoopbackUrl(url: string): boolean {
	let host: string;
	try {
		host = new URL(url).hostname;
	} catch {
		return false;
	}
	// URL wraps IPv6 in brackets in .host but strips them in .hostname; normalize anyway.
	const h = host.replace(/^\[|\]$/g, "").toLowerCase();
	if (h === "localhost" || h === "::1") return true;
	return /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(h);
}

/**
 * The cookie-sealing key for h3 `useSession` (must be ≥ 32 chars). Precedence:
 *   1. PUNKTFUNK_UI_SECRET — explicit operator override.
 *   2. Derived from the MANAGEMENT TOKEN (a 32-byte / 64-hex CSPRNG value) — the packaged deployment
 *      always has one, so the seal key is high-entropy without any extra config.
 *   3. Only as a last resort (dev/local with no token) derive from the password.
 *
 * Why not (2)→password by default: the password is low-entropy (a human picks it), so a key DERIVED
 * from it turns any captured session cookie into an OFFLINE dictionary oracle — an attacker unseals
 * candidate cookies locally, no server round-trips, so the login throttle can't help. The mgmt token
 * is unguessable, so a cookie sealed under it leaks nothing about the password. (Deriving from the
 * token instead of the password also means changing the password no longer silently invalidates
 * sessions; rotating the mgmt token does — the correct, security-relevant trigger.)
 */
export function sessionConfig(): SessionConfig {
	const explicit = process.env.PUNKTFUNK_UI_SECRET;
	const token = mgmtToken();
	let secret: string;
	if (explicit && explicit.length >= 32) {
		secret = explicit;
	} else if (token) {
		// High-entropy source: the CSPRNG mgmt token. Hash it (never use the raw admin token as the
		// seal key) with a distinct label so the two uses can't be conflated.
		secret = createHash("sha256")
			.update(`punktfunk-session-v1:token:${token}`)
			.digest("hex");
	} else {
		// Last resort (no token configured — dev/local only). No worse than before; a real deployment
		// always has a token and never reaches here.
		secret = createHash("sha256")
			.update(`punktfunk-session-v1:${uiPassword()}`)
			.digest("hex");
	}
	return {
		name: SESSION_NAME,
		// h3's `useSession` calls this seal key `password` (it's the iron/AES-GCM key, not the login
		// password — see the derivation above).
		password: secret,
		// Bounds a stolen/replayed cookie's lifetime (sets the cookie Max-Age AND the iron
		// seal TTL). 7 days for a single-user console.
		maxAge: 60 * 60 * 24 * 7,
		cookie: {
			httpOnly: true,
			sameSite: "lax",
			path: "/",
			// h3 defaults Secure to true, which browsers DROP over plain http:// (so login
			// silently fails on a LAN HTTP server). Only mark Secure when actually behind TLS.
			//
			// Derived from whether TLS is CONFIGURED, not from `PUNKTFUNK_UI_SECURE` alone
			// (2026-08-05 review L-20). The entry point already refuses the inverse mistake —
			// `PUNKTFUNK_UI_SECURE` without TLS exits rather than serving a console whose cookie
			// the browser will never store — but nothing caught this direction: TLS configured and
			// the flag forgotten shipped a session cookie without `Secure`, which a browser will
			// then also send over a plain-http downgrade. The env var still forces it on for a
			// deploy terminating TLS in front of us (a reverse proxy), where this process sees no
			// cert of its own.
			secure:
				(!!process.env.PUNKTFUNK_UI_TLS_CERT &&
					!!process.env.PUNKTFUNK_UI_TLS_KEY) ||
				/^(1|true)$/i.test(process.env.PUNKTFUNK_UI_SECURE ?? ""),
		},
	};
}

/** Constant-time string comparison (avoids leaking the password via timing). */
export function timingSafeEqual(a: string, b: string): boolean {
	const ab = Buffer.from(a);
	const bb = Buffer.from(b);
	if (ab.length !== bb.length) return false;
	return nodeTimingSafeEqual(ab, bb);
}

/** Paths reachable WITHOUT a session: the login page, the auth endpoints, and the build's
 * static assets (the login page needs its own CSS/JS, all of which live under /assets/).
 * Everything else — crucially ALL of /api — is gated.
 *
 * Note: do NOT allowlist by file extension. The client assets are all under /assets/, and a
 * generic `*.json` allowlist would expose `/api/v1/openapi.json` (and any future
 * `.json`/`.png` management route) through the proxy unauthenticated. */
export function isPublicPath(pathname: string): boolean {
	if (pathname === "/api" || pathname.startsWith("/api/")) return false; // always gated
	if (pathname === "/login") return true;
	if (pathname.startsWith("/_auth/")) return true;
	if (pathname.startsWith("/assets/")) return true;
	if (pathname === "/favicon.ico" || pathname === "/robots.txt") return true;
	// The web manifest must be fetchable to install the app, and it says nothing a logged-out
	// visitor cannot already see from the login page (name, colours, the brand mark).
	if (pathname === "/manifest.webmanifest") return true;
	return false;
}

/**
 * Collapse a request path to the shape an upstream router will actually see: percent-decoded,
 * with empty (`//`) and `.` segments dropped and `..` resolved. Used to test denylists against
 * something an attacker cannot re-spell — `/api//v1/x`, `/api/./v1/x` and `/api/v1/%78` all reach
 * the same handler, so matching only the literal path is not a security boundary.
 *
 * Decoding is per segment and failure-tolerant: a malformed escape keeps the raw segment rather
 * than throwing, so a bad path degrades to "does not match the canonical form" instead of a 500.
 */
export function normalizePath(pathname: string): string {
	const out: string[] = [];
	for (const raw of pathname.split("/")) {
		let seg = raw;
		try {
			seg = decodeURIComponent(raw);
		} catch {
			// Malformed escape — keep the raw segment.
		}
		if (seg === "" || seg === ".") continue;
		if (seg === "..") {
			out.pop();
			continue;
		}
		out.push(seg);
	}
	return `/${out.join("/")}`;
}

/** Validate a post-login redirect target: a same-origin path only. Resolves `next` against a
 * sentinel origin and keeps it only if it stays same-origin — rejecting absolute (`https://evil.com`),
 * protocol-relative (`//evil.com`) AND backslash/tab variants (`/\evil.com`, which the WHATWG URL
 * parser folds to `//evil.com`) that a plain `startsWith("//")` guard lets through. */
export function safeNextPath(next: string | undefined): string {
	if (!next) return "/";
	try {
		const base = "http://pf.invalid";
		const u = new URL(next, base);
		return u.origin === base ? u.pathname + u.search + u.hash : "/";
	} catch {
		return "/";
	}
}

export interface SessionData {
	authenticated?: boolean;
	/** The epoch this session was sealed under — see `sessionEpoch`. */
	epoch?: number;
}
