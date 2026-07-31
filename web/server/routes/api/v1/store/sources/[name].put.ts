// PUT /api/v1/store/sources/{name} — adding or repointing a catalog source is a TRUST-ROOT change:
// every future install from that source is admitted on its say-so, and `public_key` is optional, so
// a source may be unsigned. That is the boundary worth a password (util/confirm.ts), not each
// individual install past it. Wins over the `/api/**` catch-all by h3 route specificity.
//
// DELETE is deliberately NOT gated — removing a source only ever narrows what the host will trust.
import { defineEventHandler, getRouterParam, readBody } from "h3";
import { confirmPassword } from "../../../../../util/confirm";
import { forwardJson } from "../../../../../util/forward";

interface SourceBody {
	url?: string;
	public_key?: string;
	password?: string;
}

export default defineEventHandler(async (event) => {
	const body = await readBody<SourceBody>(event);
	confirmPassword(event, body?.password);
	const name = getRouterParam(event, "name") ?? "";
	// Rebuild the body from known fields so the password cannot leak upstream.
	const upstream: { url: string; public_key?: string } = {
		url: String(body?.url ?? ""),
	};
	const key = body?.public_key?.trim();
	if (key) upstream.public_key = key;
	return forwardJson(
		event,
		`/api/v1/store/sources/${encodeURIComponent(name)}`,
		"PUT",
		upstream,
	);
});
