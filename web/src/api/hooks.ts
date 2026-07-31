// Automation (event hooks). Read uses the generated query; the WRITE is hand-rolled because it
// carries the console password, which the BFF verifies and strips
// (server/routes/api/v1/hooks.put.ts) — a hook is a shell command the host will run on its own
// events, so a session cookie alone must not be able to install one.
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { apiFetch } from "@/api/fetcher";
import { getGetHooksQueryKey } from "@/api/gen/hooks/hooks";
import type { HookEntry } from "@/api/gen/model/hookEntry";

/** The whole automation config is written at once — the host has no per-hook route. */
export function useSaveHooks() {
	const qc = useQueryClient();
	return useMutation({
		mutationFn: ({
			hooks,
			password,
		}: {
			hooks: HookEntry[];
			password: string;
		}) =>
			apiFetch<void>("/api/v1/hooks", {
				method: "PUT",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({ hooks, password }),
			}),
		onSuccess: () => {
			qc.invalidateQueries({ queryKey: getGetHooksQueryKey() });
		},
	});
}

/** A one-line description of what a hook does, for the list row. */
export function hookAction(h: HookEntry): string {
	if (h.run) return h.run;
	if (h.webhook) return h.webhook;
	return "";
}

/** Human summary of a hook's filter, or "" when it matches everything. */
export function hookFilterSummary(h: HookEntry): string {
	const f = h.filter;
	if (!f) return "";
	return [
		f.client && `client=${f.client}`,
		f.app && `app=${f.app}`,
		f.plane && `plane=${f.plane}`,
		f.fingerprint && `fp=${f.fingerprint.slice(0, 12)}…`,
	]
		.filter(Boolean)
		.join(" · ");
}
