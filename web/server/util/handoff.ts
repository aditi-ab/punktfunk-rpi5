// Verification for the console handoff ticket — the thing that lets `punktfunk-host ctl
// console-url` open the console already logged in.
//
// Split out of the route so it is testable without an h3 event, because the failure mode here is
// silent in the worst direction: a verifier that is too lax hands a session to anyone who can guess
// a URL shape. The route owns the cookie; this file owns the decision.
//
// The ticket is minted by the host in `crates/punktfunk-host/src/ctl.rs::console_url` and both
// sides key the HMAC with the **management token** — a 0600 file in the host's 0700 config dir.
// That is the whole trust argument: somebody who can read it can already drive the admin API
// directly, so proving they can read it is not a lower bar than the password, it is the same bar
// reached a different way.

/** How long a ticket stays valid. Long enough for a browser cold start, short enough that one seen
 * in `ps` or a shell history is already dead. Shared with the Rust side only by being documented —
 * the host does not encode an expiry, it just stamps the time. */
export const HANDOFF_TTL_MS = 60_000;

export type HandoffVerdict =
	| { ok: true }
	| { ok: false; reason: "malformed" | "expired" | "replayed" | "bad-signature" };

/** The signed message. Kept in one place because it is a cross-language contract: change it here
 * and `console_url` in the host must change in the same commit. */
export function handoffMessage(ts: string, nonce: string): string {
	return `pf-console-handoff:v1:${ts}:${nonce}`;
}

async function macFor(key: string, ts: string, nonce: string): Promise<string> {
	const enc = new TextEncoder();
	const cryptoKey = await crypto.subtle.importKey(
		"raw",
		enc.encode(key),
		{ name: "HMAC", hash: "SHA-256" },
		false,
		["sign"],
	);
	const sig = await crypto.subtle.sign(
		"HMAC",
		cryptoKey,
		enc.encode(handoffMessage(ts, nonce)),
	);
	return [...new Uint8Array(sig)]
		.map((b) => b.toString(16).padStart(2, "0"))
		.join("");
}

/** Constant-time compare of two equal-length hex strings. Length is not secret (it is fixed by the
 * hash), so returning early on a mismatched length leaks nothing. */
export function safeEqualHex(a: string, b: string): boolean {
	if (a.length !== b.length) return false;
	let diff = 0;
	for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
	return diff === 0;
}

/**
 * Decide whether `ticket` may open a session.
 *
 * `seen` is the caller's replay set (ticket → redeemed-at ms); this function reads AND records, so
 * a second call with the same ticket is refused. Passing a fresh map per request would therefore
 * disable single-use — the route keeps one for the process lifetime on purpose.
 */
export async function verifyHandoff(
	ticket: string,
	key: string,
	seen: Map<string, number>,
	now: number = Date.now(),
): Promise<HandoffVerdict> {
	for (const [t, at] of seen) if (now - at > HANDOFF_TTL_MS) seen.delete(t);

	const parts = ticket.split(".");
	if (parts.length !== 3 || parts.some((p) => p.length === 0)) {
		return { ok: false, reason: "malformed" };
	}
	const [ts, nonce, mac] = parts;
	if (!/^\d+$/.test(ts) || !/^[0-9a-f]+$/.test(nonce) || !/^[0-9a-f]+$/.test(mac)) {
		return { ok: false, reason: "malformed" };
	}

	const issued = Number(ts) * 1000;
	// Symmetric window: a ticket from the future is as wrong as an old one, and clock skew between
	// two processes on the SAME box is not a thing we need to forgive.
	if (!Number.isFinite(issued) || Math.abs(now - issued) > HANDOFF_TTL_MS) {
		return { ok: false, reason: "expired" };
	}
	if (seen.has(ticket)) return { ok: false, reason: "replayed" };
	if (!safeEqualHex(mac, await macFor(key, ts, nonce))) {
		return { ok: false, reason: "bad-signature" };
	}
	seen.set(ticket, now);
	return { ok: true };
}
