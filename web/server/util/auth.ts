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
import type { SessionConfig } from "h3";

export const SESSION_NAME = "pf_session";

/** The login password. Empty string ⇒ auth is MISCONFIGURED (the gate fails closed). */
export function uiPassword(): string {
	return process.env.PUNKTFUNK_UI_PASSWORD ?? "";
}

/** The management API the proxy forwards to (loopback by default — never LAN-exposed). It serves
 * HTTPS with the host's self-signed identity cert, so the deployment also sets
 * NODE_TLS_REJECT_UNAUTHORIZED=0 for the (loopback-only) proxy fetch — see .env.example. */
export function mgmtUrl(): string {
	return process.env.PUNKTFUNK_MGMT_URL ?? "https://127.0.0.1:47990";
}

/** Bearer token for the management API, injected server-side. */
export function mgmtToken(): string {
	return process.env.PUNKTFUNK_MGMT_TOKEN ?? "";
}

/**
 * The cookie-sealing key for h3 `useSession` (must be ≥ 32 chars). Use PUNKTFUNK_UI_SECRET
 * if set; otherwise derive a stable 64-hex key from the password so single-var config works
 * (changing the password then invalidates existing sessions, which is fine).
 */
export function sessionConfig(): SessionConfig {
	const secret = process.env.PUNKTFUNK_UI_SECRET;
	const password =
		secret && secret.length >= 32
			? secret
			: createHash("sha256")
					.update(`punktfunk-session-v1:${uiPassword()}`)
					.digest("hex");
	return {
		name: SESSION_NAME,
		password,
		// Bounds a stolen/replayed cookie's lifetime (sets the cookie Max-Age AND the iron
		// seal TTL). 7 days for a single-user console.
		maxAge: 60 * 60 * 24 * 7,
		cookie: {
			httpOnly: true,
			sameSite: "lax",
			path: "/",
			// h3 defaults Secure to true, which browsers DROP over plain http:// (so login
			// silently fails on a LAN HTTP server). Only mark Secure when actually behind TLS
			// (set PUNKTFUNK_UI_SECURE=1 / =true then).
			secure: /^(1|true)$/i.test(process.env.PUNKTFUNK_UI_SECURE ?? ""),
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
	return false;
}

/** Validate a post-login redirect target: a same-origin path only. Rejects protocol-
 * relative (`//evil.com`) and absolute URLs to prevent an open redirect. */
export function safeNextPath(next: string | undefined): string {
	if (!next || !next.startsWith("/") || next.startsWith("//")) return "/";
	return next;
}

export interface SessionData {
	authenticated?: boolean;
}
