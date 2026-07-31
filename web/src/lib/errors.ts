import { ApiError } from "@/api/fetcher";

/**
 * The server's own `{ error }` message from a thrown `ApiError` (its `.data` body), for inline
 * display — falling back to the HTTP status text, then to whatever was thrown.
 *
 * The host writes genuinely useful refusals ("entry is owned by provider `x` — update it through
 * its reconcile"), and showing a generic "something went wrong" in their place throws away the one
 * piece of information that tells the operator what to do next.
 *
 * Lives here rather than in a section because several of them need it; it started life private to
 * the display card.
 */
export function apiErrorMessage(err: unknown): string | undefined {
	if (err instanceof ApiError) {
		const data = err.data as { error?: string } | undefined;
		return data?.error ?? err.message;
	}
	return err ? String(err) : undefined;
}
