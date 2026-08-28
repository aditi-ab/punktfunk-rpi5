// Shared password gate for the library writes that carry the SAME primitive `hooks.put.ts` gates.
//
// A custom library entry can carry `prep` (commands run before the title launches) and a
// `launch.kind === "command"` (a shell command run at launch). Both are executed verbatim as the
// host user — `/bin/sh -c` on Linux, `cmd.exe /c` on Windows — which is the very thing the hooks
// gate exists to stop a bare session cookie from doing: *"a 7-day session cookie must not be enough
// to leave a persistent command behind on the machine."*
//
// `confirm.ts` gated three routes and these were not among them, so the identical primitive fell
// through the ungated `/api/**` catch-all where the BFF attaches the admin bearer unconditionally
// (2026-08-05 review M-6). Anyone with a session cookie but not the password — a borrowed browser,
// an exfiltrated cookie, a stale 7-day session after a password rotation — could leave a command
// behind. `SameSite=lax` blocks a plain cross-site POST, so this is cookie possession rather than
// drive-by CSRF, but the invariant is the same one.
//
// The gate is CONDITIONAL on the payload actually carrying one of those fields. An ordinary library
// edit — title, artwork, platform, a `steam_appid` launch — is not code execution and prompting for
// it would only train the operator to type their password without reading it. Same reasoning as
// "a catalog install from an already-trusted source is deliberately NOT gated" in `confirm.ts`.
import type { H3Event } from "h3";
import { confirmPassword } from "./confirm";

/** The shape the gate inspects; everything else about the entry is none of its business. */
interface EntryLike {
	prep?: unknown;
	launch?: { kind?: unknown } | null;
}

/** Does this entry carry a field the host will hand to a shell? */
export function carriesCommandExecution(
	entry: EntryLike | null | undefined,
): boolean {
	if (!entry || typeof entry !== "object") return false;
	if (Array.isArray(entry.prep) && entry.prep.length > 0) return true;
	return entry.launch?.kind === "command";
}

/**
 * Re-verify the console password iff `entries` contains a command-execution field. Throws the same
 * 401/429/503 `confirmPassword` does; returns normally when the gate does not apply.
 */
export function confirmIfCommandExecution(
	event: H3Event,
	entries: EntryLike | EntryLike[] | null | undefined,
	password: unknown,
): void {
	const list = Array.isArray(entries) ? entries : [entries];
	if (list.some(carriesCommandExecution)) confirmPassword(event, password);
}
