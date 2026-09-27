import { ApiError } from "@/api/fetcher";

/**
 * The server's own message from a thrown `ApiError`, for inline display: the host's `{ error }`
 * string, else the `statusMessage` of a refusal the BFF throws through h3 (whose body carries
 * `error: true`), else the HTTP status text, else whatever was thrown.
 *
 * The host writes genuinely useful refusals ("entry is owned by provider `x` — update it through
 * its reconcile"), and a generic "something went wrong" in their place throws away the one piece
 * of information that tells the operator what to do next.
 */
export function apiErrorMessage(err: unknown): string | undefined {
	if (err instanceof ApiError) {
		const data = err.data as
			| { error?: unknown; statusMessage?: string }
			| undefined;
		if (typeof data?.error === "string") return data.error;
		return data?.statusMessage || err.message;
	}
	return err ? String(err) : undefined;
}
