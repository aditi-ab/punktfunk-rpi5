import { Gamepad2, XCircle } from "lucide-react";
import type { FC } from "react";
import type { ActiveGame } from "@/api/gen/model/activeGame";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { m } from "@/paraglide/messages";

/**
 * What the host is running, and — when a client has vanished — how long its game has left.
 *
 * The `grace` rows are the reason this card exists at all. A game whose session is gone is on a
 * countdown to being closed, which costs unsaved progress; that has to be visible somewhere, with a
 * way to say "yes, do it now" or (by reconnecting) "no". The live rows come almost free alongside.
 */
export const RunningGames: FC<{
	games: ActiveGame[];
	/** The catalog, for box art — matched by `app_id`. Absent while `/library` is still loading. */
	library?: GameEntry[];
	onEnd: (game: ActiveGame) => void;
	isEnding: boolean;
}> = ({ games, library, onEnd, isEnding }) => {
	if (games.length === 0) return null;
	return (
		<Card>
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<Gamepad2 className="size-4" />
					{m.games_title()}
				</CardTitle>
			</CardHeader>
			<CardContent className="flex flex-col gap-3">
				{games.map((g) => (
					<GameRow
						// A host can run the same title for two clients at once (native admits concurrent
						// sessions), so the title alone is not a key.
						key={`${g.plane}:${g.session_id ?? "grace"}:${g.app_id ?? g.title}`}
						game={g}
						art={coverFor(g, library)}
						onEnd={() => onEnd(g)}
						isEnding={isEnding}
					/>
				))}
			</CardContent>
		</Card>
	);
};

const GameRow: FC<{
	game: ActiveGame;
	art?: string;
	onEnd: () => void;
	isEnding: boolean;
}> = ({ game, art, onEnd, isEnding }) => {
	const waiting = game.state === "grace";
	return (
		<div className="flex items-center gap-3">
			<div className="h-16 w-11 shrink-0 overflow-hidden rounded bg-muted">
				{art && (
					<img
						src={art}
						alt=""
						loading="lazy"
						className="size-full object-cover"
					/>
				)}
			</div>
			<div className="min-w-0 flex-1">
				<div className="flex flex-wrap items-center gap-2">
					<span className="truncate font-medium">{game.title}</span>
					<Badge variant={waiting ? "destructive" : stateVariant(game.state)}>
						{stateLabel(game.state)}
					</Badge>
				</div>
				<p className="mt-0.5 truncate text-xs text-muted-foreground">
					{waiting
						? m.games_closing_in({
								time: formatCountdown(game.grace_remaining_s ?? 0),
							})
						: [game.client, planeLabel(game.plane)].filter(Boolean).join(" · ")}
				</p>
			</div>
			<Button
				variant={waiting ? "destructive" : "outline"}
				size="sm"
				disabled={isEnding}
				onClick={onEnd}
			>
				<XCircle className="size-3.5" />
				{m.games_end_now()}
			</Button>
		</div>
	);
};

/**
 * Box art for a running game, matched against the already-fetched catalog by store-qualified id —
 * so this card costs no extra request. `undefined` for an operator-typed GameStream command (no
 * catalog entry behind it) or a title whose entry carries no art.
 */
function coverFor(game: ActiveGame, library?: GameEntry[]): string | undefined {
	if (!game.app_id || !library) return undefined;
	const entry = library.find((e) => e.id === game.app_id);
	return entry?.art.portrait ?? entry?.art.header ?? undefined;
}

/** `mm:ss` — the countdown reads as a duration, not a number of seconds. */
function formatCountdown(seconds: number): string {
	const s = Math.max(0, Math.floor(seconds));
	return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function stateLabel(state: string): string {
	switch (state) {
		case "launching":
			return m.games_state_launching();
		case "running":
			return m.games_state_running();
		case "exited":
			return m.games_state_exited();
		case "grace":
			return m.games_state_grace();
		default:
			return state;
	}
}

function stateVariant(state: string): "success" | "secondary" | "outline" {
	if (state === "running") return "success";
	if (state === "launching") return "secondary";
	return "outline";
}

/** The compat plane is worth naming: it is why a row may show an IP instead of a device name. */
function planeLabel(plane: string): string {
	return plane === "gamestream" ? "GameStream" : "";
}
