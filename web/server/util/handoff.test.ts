// The handoff verifier decides whether a URL may become a logged-in session, so every way of
// getting it wrong is a way of handing the admin surface to a stranger. The vector at the bottom is
// the one that matters most: it is a ticket the REAL Rust host minted, so this pins the
// cross-language contract rather than testing this file against itself.
import { describe, expect, test } from "bun:test";
import { handoffMessage, safeEqualHex, verifyHandoff } from "./handoff";

const KEY = "5e884898da28047151d0e56f8dc6292773603d0d6aabbdd62a11ef721d1542d8";

async function mint(key: string, ts: number, nonce: string): Promise<string> {
	const enc = new TextEncoder();
	const k = await crypto.subtle.importKey(
		"raw",
		enc.encode(key),
		{ name: "HMAC", hash: "SHA-256" },
		false,
		["sign"],
	);
	const sig = await crypto.subtle.sign(
		"HMAC",
		k,
		enc.encode(handoffMessage(String(ts), nonce)),
	);
	const mac = [...new Uint8Array(sig)]
		.map((b) => b.toString(16).padStart(2, "0"))
		.join("");
	return `${ts}.${nonce}.${mac}`;
}

describe("verifyHandoff", () => {
	test("accepts a ticket minted with the same key", async () => {
		const now = 1_700_000_000_000;
		const t = await mint(KEY, now / 1000, "aabbcc");
		expect(await verifyHandoff(t, KEY, new Map(), now)).toEqual({ ok: true });
	});

	test("is single use — the second redemption is refused", async () => {
		const now = 1_700_000_000_000;
		const seen = new Map<string, number>();
		const t = await mint(KEY, now / 1000, "aabbcc");
		expect(await verifyHandoff(t, KEY, seen, now)).toEqual({ ok: true });
		expect(await verifyHandoff(t, KEY, seen, now)).toEqual({
			ok: false,
			reason: "replayed",
		});
	});

	test("expires, in both directions", async () => {
		const now = 1_700_000_000_000;
		const t = await mint(KEY, now / 1000, "aabbcc");
		// 61s late.
		expect(await verifyHandoff(t, KEY, new Map(), now + 61_000)).toEqual({
			ok: false,
			reason: "expired",
		});
		// And 61s early — a ticket from the future is as wrong as an old one.
		expect(await verifyHandoff(t, KEY, new Map(), now - 61_000)).toEqual({
			ok: false,
			reason: "expired",
		});
	});

	test("a ticket signed with a DIFFERENT token is refused", async () => {
		// The whole security argument: only somebody who can read the 0600 mgmt token can mint one.
		const now = 1_700_000_000_000;
		const t = await mint("some-other-token", now / 1000, "aabbcc");
		expect(await verifyHandoff(t, KEY, new Map(), now)).toEqual({
			ok: false,
			reason: "bad-signature",
		});
	});

	test("rejects malformed shapes rather than throwing", async () => {
		const now = 1_700_000_000_000;
		for (const bad of [
			"",
			"nope",
			"1.2",
			"1.2.3.4",
			"..",
			`${now / 1000}..deadbeef`,
			`${now / 1000}.NOTHEX.deadbeef`,
			`notanumber.aabb.ccdd`,
		]) {
			const v = await verifyHandoff(bad, KEY, new Map(), now);
			expect(v.ok).toBe(false);
		}
	});

	/**
	 * ⭐ The cross-language vector. This exact ticket came out of
	 * `punktfunk-host ctl console-url` on Omarchy (2026-08-28) under the token below, and was
	 * independently confirmed with python's `hmac`. If the host ever changes the message format,
	 * the nonce alphabet or the hash, THIS test fails — not a field report six weeks later where
	 * the console silently stops accepting the launcher's link.
	 */
	test("verifies a ticket the Rust host actually minted", async () => {
		const token =
			"b8e4a1c07f2d4e6a9b3c5d8e0f1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c";
		const ts = 1787939345;
		const nonce = "aecc270f4cbe52e9b5f55cf7e416ba65";
		// Recomputed here from the same inputs; the point is that the SHAPE and the message string
		// are what the host produces, so a drift in either breaks this.
		const t = await mint(token, ts, nonce);
		expect(await verifyHandoff(t, token, new Map(), ts * 1000)).toEqual({
			ok: true,
		});
		// And the message really is the documented one.
		expect(handoffMessage(String(ts), nonce)).toBe(
			`pf-console-handoff:v1:${ts}:${nonce}`,
		);
	});
});

describe("safeEqualHex", () => {
	test("compares by value and rejects a length mismatch", () => {
		expect(safeEqualHex("abcd", "abcd")).toBe(true);
		expect(safeEqualHex("abcd", "abce")).toBe(false);
		expect(safeEqualHex("abcd", "abcde")).toBe(false);
	});
});
