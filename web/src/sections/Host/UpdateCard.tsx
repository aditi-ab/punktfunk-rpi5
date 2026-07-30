import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@unom/ui/button";
import { type FC, type ReactNode, useState } from "react";
import {
	getGetUpdateStatusQueryKey,
	useForceUpdateCheck,
	useGetUpdateStatus,
} from "@/api/gen/update/update";
import type { UpdateStatus } from "@/api/gen/model";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";

/**
 * Container: the host update-check card (U0 — notify-only). Reading status is what keeps the
 * host's manifest cache warm (the host kicks its own background refresh when the cache is >6 h
 * old), so a modest poll doubles as the check cadence while the console is open. Apply buttons
 * arrive with the per-channel apply legs (U1 Windows, U2 Linux helper); until then the card's
 * action is the exact update command for this install kind.
 */
export const UpdateSection: FC = () => {
	const qc = useQueryClient();
	const status = useGetUpdateStatus({ query: { refetchInterval: 60_000 } });
	const check = useForceUpdateCheck();

	const checkNow = () =>
		check.mutate(undefined, {
			onSuccess: (fresh) => {
				qc.setQueryData(getGetUpdateStatusQueryKey(), fresh);
			},
		});

	return <UpdateCard state={status} onCheck={checkNow} busy={check.isPending} />;
};

export const UpdateCard: FC<{
	state: Loadable<UpdateStatus>;
	onCheck: () => void;
	busy: boolean;
}> = ({ state, onCheck, busy }) => {
	const s = state.data;
	return (
		<Card>
			<CardHeader className="flex flex-row items-center justify-between">
				<CardTitle>{m.update_title()}</CardTitle>
				{s?.available && <Badge>{m.update_available_badge()}</Badge>}
			</CardHeader>
			<CardContent className="space-y-4">
				<QueryState
					isLoading={state.isLoading}
					error={state.error}
					refetch={state.refetch}
				>
					{s && (
						<>
							<dl className="grid grid-cols-1 gap-3">
								<UpdateRow
									label={m.update_current()}
									value={
										<span className="flex items-center gap-2 font-medium">
											{s.current_version}
											<Badge variant="secondary">{s.channel}</Badge>
											<Badge variant="outline" title={m.update_install_kind()}>
												{s.install_kind}
											</Badge>
										</span>
									}
								/>
								<UpdateRow
									label={m.update_latest()}
									value={
										s.manifest ? (
											<span className="flex items-center gap-2 font-medium">
												{s.manifest.version}
												{s.manifest.notes_url && (
													<a
														href={s.manifest.notes_url}
														target="_blank"
														rel="noreferrer"
														className="text-sm font-normal underline underline-offset-2"
													>
														{m.update_notes()}
													</a>
												)}
											</span>
										) : (
											<span className="text-sm text-muted-foreground">
												{m.update_never_checked()}
											</span>
										)
									}
								/>
							</dl>

							{s.available ? (
								<div className="space-y-2 rounded-md border p-4">
									<p className="text-sm">{m.update_how()}</p>
									<CommandLine command={s.channel_hint} />
								</div>
							) : (
								s.manifest && (
									<p className="text-sm text-muted-foreground">
										{m.update_up_to_date()}
									</p>
								)
							)}

							{s.manifest?.stale && (
								<p className="rounded-md border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
									{m.update_stale()}
								</p>
							)}
							{s.last_error && (
								<p className="text-sm text-destructive">
									{m.update_error()} {s.last_error}
								</p>
							)}
							{s.check_disabled ? (
								<p className="text-sm text-muted-foreground">
									{m.update_disabled()}
								</p>
							) : (
								<div className="flex items-center gap-3">
									<Button
										variant="outline"
										size="sm"
										onClick={onCheck}
										disabled={busy}
									>
										{busy ? m.update_checking() : m.update_check_now()}
									</Button>
									{s.last_checked_unix != null && (
										<span className="text-xs text-muted-foreground">
											{m.update_last_checked()}{" "}
											{new Date(s.last_checked_unix * 1000).toLocaleString()}
										</span>
									)}
								</div>
							)}
						</>
					)}
				</QueryState>
			</CardContent>
		</Card>
	);
};

const UpdateRow: FC<{ label: string; value: ReactNode }> = ({
	label,
	value,
}) => (
	<div className="flex items-baseline justify-between gap-4">
		<dt className="text-sm text-muted-foreground">{label}</dt>
		<dd>{value}</dd>
	</div>
);

/** The copy-pastable update command, with a small clipboard affordance. */
const CommandLine: FC<{ command: string }> = ({ command }) => {
	const [copied, setCopied] = useState(false);
	const copy = () => {
		navigator.clipboard
			.writeText(command)
			.then(() => {
				setCopied(true);
				setTimeout(() => setCopied(false), 2000);
			})
			.catch(() => {
				/* clipboard denied — the text is selectable, nothing to do */
			});
	};
	return (
		<div className="flex items-center gap-2">
			<code className="min-w-0 flex-1 overflow-x-auto rounded bg-muted px-2 py-1.5 text-xs">
				{command}
			</code>
			<Button variant="ghost" size="sm" onClick={copy}>
				{copied ? m.update_copied() : m.update_copy()}
			</Button>
		</div>
	);
};
