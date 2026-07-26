import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@unom/ui/button";
import { toast } from "@unom/ui/toast";
import { type FC, type ReactNode, useEffect, useState } from "react";
import { ApiError } from "@/api/fetcher";
import type { GameOnSessionEnd, SessionSettings } from "@/api/gen/model";
import {
	getGetSessionSettingsQueryKey,
	useGetSessionSettings,
	useSetSessionSettings,
} from "@/api/gen/session/session";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

const END_POLICIES: GameOnSessionEnd[] = ["keep", "on_quit", "always"];

/**
 * Whether a launched game and its streaming session share a fate
 * (design/session-game-lifetime.md), next to the display keep-alive policy because the two interact:
 * a kept display and a kept game are separate decisions with separate timers, and `keep_alive:
 * forever` outranks the game policy for the display itself.
 *
 * Every axis saves on change (like the display policy above) — there is no Save button to miss.
 */
export const SessionGameCard: FC = () => {
	const qc = useQueryClient();
	const q = useGetSessionSettings();
	const save = useSetSessionSettings();
	const server = q.data?.settings;
	// Which axes this build acts on. Empty on a platform with no launch path (macOS), where the
	// controls are shown disabled rather than hidden — "does nothing here" is information.
	const enforced = q.data?.enforced ?? [];
	const acts = (field: string) =>
		enforced.length === 0 || enforced.includes(field);

	// The grace field is free text while being typed, so it gets a local buffer; the other two axes
	// are discrete and go straight to the host.
	const [grace, setGrace] = useState("");
	useEffect(() => {
		if (server) setGrace(String(server.disconnect_grace_seconds ?? 300));
	}, [server]);

	const apply = (patch: Partial<SessionSettings>) => {
		if (!server) return;
		save.mutate(
			{ data: { ...server, ...patch } },
			{
				onSuccess: () => {
					qc.invalidateQueries({ queryKey: getGetSessionSettingsQueryKey() });
					toast.success(m.session_game_saved());
				},
			},
		);
	};

	const busy = save.isPending;
	const error = save.error instanceof ApiError ? save.error.message : undefined;

	return (
		<Card>
			<CardHeader>
				<CardTitle>{m.session_game_title()}</CardTitle>
			</CardHeader>
			<CardContent className="space-y-4">
				<p className="max-w-prose text-sm text-muted-foreground">
					{m.session_game_help()}
				</p>
				<QueryState isLoading={q.isLoading} error={q.error} refetch={q.refetch}>
					{server && (
						<div className="space-y-6">
							<Field
								label={m.session_game_on_exit()}
								help={m.session_game_on_exit_help()}
							>
								<div className="flex flex-wrap gap-2">
									<Choice
										selected={server.session_on_game_exit === true}
										disabled={busy || !acts("session_on_game_exit")}
										onClick={() => apply({ session_on_game_exit: true })}
									>
										{m.session_game_on_exit_end()}
									</Choice>
									<Choice
										selected={server.session_on_game_exit === false}
										disabled={busy || !acts("session_on_game_exit")}
										onClick={() => apply({ session_on_game_exit: false })}
									>
										{m.session_game_on_exit_keep()}
									</Choice>
								</div>
							</Field>

							<Field
								label={m.session_game_end_game()}
								help={m.session_game_end_game_help()}
							>
								<div className="flex flex-wrap gap-2">
									{END_POLICIES.map((p) => (
										<Choice
											key={p}
											selected={(server.game_on_session_end ?? "keep") === p}
											disabled={busy || !acts("game_on_session_end")}
											onClick={() => apply({ game_on_session_end: p })}
										>
											{END_POLICY_LABEL[p]()}
										</Choice>
									))}
								</div>
								{(server.game_on_session_end ?? "keep") === "always" && (
									<p className="max-w-prose text-xs text-muted-foreground">
										{m.session_game_always_warning()}
									</p>
								)}
							</Field>

							{(server.game_on_session_end ?? "keep") === "always" && (
								<Field
									label={m.session_game_grace()}
									help={m.session_game_grace_help()}
								>
									<div className="flex items-center gap-2">
										<Input
											type="number"
											min={10}
											max={86400}
											className="w-28"
											value={grace}
											disabled={busy || !acts("disconnect_grace_seconds")}
											onChange={(e) => setGrace(e.target.value)}
											onBlur={() => {
												const n = Number(grace);
												if (!Number.isFinite(n)) {
													setGrace(
														String(server.disconnect_grace_seconds ?? 300),
													);
													return;
												}
												// The host clamps to 10..=86400 and returns what it stored, so
												// a nonsense number is corrected rather than rejected.
												if (n !== server.disconnect_grace_seconds) {
													apply({ disconnect_grace_seconds: n });
												}
											}}
										/>
										<span className="text-sm text-muted-foreground">
											{m.display_keep_alive_seconds()}
										</span>
									</div>
								</Field>
							)}

							{enforced.length === 0 && (
								<Badge variant="outline">{m.session_game_inert()}</Badge>
							)}
							{error && <p className="text-sm text-destructive">{error}</p>}
						</div>
					)}
				</QueryState>
			</CardContent>
		</Card>
	);
};

const END_POLICY_LABEL: Record<GameOnSessionEnd, () => string> = {
	keep: () => m.session_game_end_keep(),
	on_quit: () => m.session_game_end_on_quit(),
	always: () => m.session_game_end_always(),
};

const Field: FC<{ label: string; help?: string; children: ReactNode }> = ({
	label,
	help,
	children,
}) => (
	<div className="space-y-3">
		<Label className="block">{label}</Label>
		{children}
		{help && (
			<p className="max-w-prose text-xs text-muted-foreground">{help}</p>
		)}
	</div>
);

const Choice: FC<{
	selected: boolean;
	disabled: boolean;
	onClick: () => void;
	children: ReactNode;
}> = ({ selected, disabled, onClick, children }) => (
	<Button
		type="button"
		variant={selected ? "default" : "outline"}
		size="sm"
		disabled={disabled}
		aria-pressed={selected}
		className={cn(disabled && "opacity-60")}
		onClick={onClick}
	>
		{children}
	</Button>
);
