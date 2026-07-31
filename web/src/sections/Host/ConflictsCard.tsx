import { AlertTriangle } from "lucide-react";
import type { FC } from "react";
import { useGetLocalSummary } from "@/api/gen/host/host";
import { Card, CardContent } from "@/components/ui/card";
import { m } from "@/paraglide/messages";

/**
 * "Something else is already listening on these ports."
 *
 * The host detects other Moonlight-compatible servers (Sunshine, Apollo, …) running on the same
 * machine at startup and reports them in `GET /local/summary` as `conflicts`. Nothing surfaced it,
 * even though it is the single most common reason a punktfunk host looks installed and working but
 * no client can reach it — two servers fighting over the same ports, with whichever won the bind
 * answering the client.
 *
 * Renders nothing at all when there is no conflict, so a healthy host sees no extra chrome.
 */
export const ConflictsCard: FC = () => {
	// Static per host boot (the host probes once at startup), so there is nothing to poll for.
	const summary = useGetLocalSummary({ query: { staleTime: 5 * 60_000 } });
	const conflicts = summary.data?.conflicts ?? [];
	if (conflicts.length === 0) return null;
	return (
		<Card className="border-amber-600/40 dark:border-amber-500/40">
			<CardContent className="flex items-start gap-3 p-card pt-card sm:pt-card">
				<AlertTriangle className="mt-0.5 size-5 shrink-0 text-amber-600 dark:text-amber-500" />
				<div className="min-w-0 flex-1 space-y-2">
					<p className="text-sm font-medium text-amber-600 dark:text-amber-500">
						{m.host_conflicts_title()}
					</p>
					<p className="max-w-prose text-sm text-muted-foreground">
						{m.host_conflicts_help()}
					</p>
					<ul className="flex flex-col gap-1">
						{conflicts.map((c) => (
							<li
								key={c}
								className="rounded-md bg-muted px-3 py-1.5 font-mono text-xs text-muted-foreground"
							>
								{c}
							</li>
						))}
					</ul>
				</div>
			</CardContent>
		</Card>
	);
};
