import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import type { FC } from "react";
import { ApiError } from "@/api/fetcher";
import {
	getGetDisplayMonitorsQueryKey,
	getGetDisplaySettingsQueryKey,
	useGetDisplayMonitors,
	useGetDisplaySettings,
	useSetDisplaySettings,
} from "@/api/gen/display/display";
import type { ApiMonitorInfo } from "@/api/gen/model";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { QueryState } from "@/components/query-state";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

/**
 * **Streamed screen** (design/per-monitor-portal-capture.md §5.3): stream one of the host's real
 * monitors instead of creating a virtual display per client.
 *
 * Deliberately a HOST-wide choice, not per-client — it is the decision of record for this feature,
 * and it is also what keeps input honest: the injector is host-lifetime and shared by every
 * concurrent session, so "which screen do absolute coordinates land on" can only have one answer.
 *
 * Saves on selection (like the policy card above) — there is no Save button to miss. The pin is a
 * field of the display policy, so it rides the same PUT.
 */
export const MonitorCard: FC = () => {
	const qc = useQueryClient();
	const monitors = useGetDisplayMonitors();
	const settings = useGetDisplaySettings();
	const save = useSetDisplaySettings();

	const policy = settings.data?.settings;
	const rows = monitors.data?.monitors ?? [];
	// The host reports the EFFECTIVE pin — an env-pinned appliance shows its real answer here even
	// though the console cannot change it.
	const pinned = monitors.data?.pinned ?? null;
	// `PUNKTFUNK_CAPTURE_MONITOR` outranks the stored policy, so a host pinned in its unit's
	// environment is read-only here: offering controls that silently lose to the env would be worse
	// than saying so.
	const envLocked = !!pinned && policy?.capture_monitor !== pinned;
	// The host says whether it can honor a pin at all. Windows enumerates its heads but has no
	// backend that can capture one (see `MonitorsResponse.pin_supported`), and this card used to
	// offer the choice anyway: the PUT persisted, nothing consumed it, and a virtual display was
	// still created on connect. Defaults to TRUE when the field is absent so an older host — which
	// only ever shipped this picker where it worked — is not retroactively locked out.
	const pinSupported = monitors.data?.pin_supported ?? true;
	// Both reasons produce the same read-only card; only the explanation above it differs.
	const locked = envLocked || !pinSupported;

	const choose = (connector: string | null) => {
		if (!policy || locked) return;
		save.mutate(
			{ data: { ...policy, capture_monitor: connector } },
			{
				onSuccess: () => {
					qc.invalidateQueries({ queryKey: getGetDisplaySettingsQueryKey() });
					qc.invalidateQueries({ queryKey: getGetDisplayMonitorsQueryKey() });
					toast.success(m.display_monitor_saved());
				},
			},
		);
	};

	const busy = save.isPending;
	const error = save.error instanceof ApiError ? save.error.message : undefined;

	const row = (
		key: string,
		selected: boolean,
		title: string,
		hint: string,
		tags?: ReturnType<typeof Badge>[],
		onSelect?: () => void,
	) => (
		<button
			key={key}
			type="button"
			disabled={busy || locked || !onSelect}
			onClick={onSelect}
			aria-pressed={selected}
			className={cn(
				"flex w-full items-start justify-between gap-4 rounded-md border p-3 text-left transition-colors",
				selected ? "border-primary bg-primary/5" : "hover:bg-muted/50",
				(busy || locked) && "cursor-not-allowed opacity-60",
			)}
		>
			<span className="flex flex-col gap-1">
				<span className="flex items-center gap-2 font-medium">
					{title}
					{tags}
				</span>
				<span className="text-sm text-muted-foreground">{hint}</span>
			</span>
		</button>
	);

	const monitorRow = (mon: ApiMonitorInfo) => {
		const tags = [
			mon.primary ? (
				<Badge key="p" variant="secondary">
					{m.display_monitor_primary()}
				</Badge>
			) : null,
			!mon.enabled ? (
				<Badge key="d" variant="outline">
					{m.display_monitor_disabled()}
				</Badge>
			) : null,
		].filter(Boolean) as ReturnType<typeof Badge>[];
		return row(
			mon.connector,
			pinned?.toLowerCase() === mon.connector.toLowerCase(),
			`${mon.connector} — ${mon.description}`,
			`${mon.mode} · ${m.display_monitor_mirror_hint()}`,
			tags,
			// A disabled head cannot be streamed (the host refuses with that reason), so don't
			// offer it as a choice — it is listed so "why isn't it here?" has an answer.
			mon.enabled && !mon.managed ? () => choose(mon.connector) : undefined,
		);
	};

	return (
		<Card>
			<CardHeader>
				<CardTitle>{m.display_monitor_title()}</CardTitle>
			</CardHeader>
			<CardContent className="space-y-4">
				<p className="max-w-prose text-sm text-muted-foreground">
					{m.display_monitor_intro()}
				</p>
				{!pinSupported && (
					<p className="text-sm text-amber-600 dark:text-amber-500">
						{m.display_monitor_unsupported()}
					</p>
				)}
				{pinSupported && envLocked && (
					<p className="text-sm text-amber-600 dark:text-amber-500">
						{m.display_monitor_env_locked()}
					</p>
				)}
				<QueryState
					isLoading={monitors.isLoading}
					error={monitors.error}
					refetch={monitors.refetch}
				>
					<div className="flex flex-col gap-2">
						{row(
							"__virtual__",
							!pinned,
							m.display_monitor_virtual(),
							m.display_monitor_virtual_hint(),
							undefined,
							() => choose(null),
						)}
						{rows.map(monitorRow)}
						{rows.length === 0 && (
							<p className="text-sm text-muted-foreground">
								{monitors.data?.error
									? m.display_monitor_unavailable()
									: m.display_monitor_none()}
							</p>
						)}
					</div>
				</QueryState>
				{error && <p className="text-sm text-destructive">{error}</p>}
			</CardContent>
		</Card>
	);
};
