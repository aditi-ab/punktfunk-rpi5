import { Eye, EyeOff, Pencil, Trash2 } from "lucide-react";
import { type FC, useState } from "react";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { LauncherIcon } from "@/components/launcher-icon";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { m } from "@/paraglide/messages";

/**
 * Display label for a store badge. Steam and custom keep their localized strings; every other store
 * (lutris, heroic, epic, …) is a proper noun shown capitalized, so new providers surface correctly
 * without a translation per store.
 */
function storeLabel(store: string): string {
	switch (store) {
		case "custom":
			return m.library_store_custom();
		case "steam":
			return m.library_store_steam();
		default:
			return store.charAt(0).toUpperCase() + store.slice(1);
	}
}

export interface GameCardProps {
	game: OperatorGameEntry;
	onEdit: () => void;
	onDelete: () => void;
	deleting: boolean;
	/** Hide this title from every play surface, or bring it back. */
	onToggleHidden: () => void;
	/** This card's hide/un-hide is in flight — only this one disables. */
	hiding: boolean;
}

/**
 * A poster tile. The cover prefers the 2:3 portrait capsule; on a load error it
 * falls back to the wide header, then to a text placeholder. Custom entries get
 * edit/delete affordances; every entry can be hidden.
 */
export const GameCard: FC<GameCardProps> = ({
	game,
	onEdit,
	onDelete,
	deleting,
	onToggleHidden,
	hiding,
}) => {
	// Hiding is available for EVERY store, unlike edit/delete: the titles most worth hiding are the
	// ones the operator cannot edit — a launcher's own scanned entries, a Proton tool, a demo. The
	// host keys the setting by the entry id and never needs to own the entry.
	const hidden = game.hidden === true;
	// Editable only if the operator actually owns this entry. A custom-store entry SYNCED by a
	// provider plugin also has `store === "custom"`, but the host refuses to hand-edit or delete it
	// (409 CONFLICT, "owned by provider … — update it through its reconcile"), so offering the
	// buttons produced a failure the card never surfaced. Provider-owned entries are attributed
	// instead.
	const isCustom = game.store === "custom" && !game.provider;
	// Track which sources have failed so the <img> can step down portrait → header → placeholder.
	const [failed, setFailed] = useState<Record<string, boolean>>({});

	const candidates = [game.art.portrait, game.art.header].filter(
		(u): u is string => !!u && !failed[u],
	);
	const src = candidates[0];
	// A launcher tile ships no cover art by design (its own icon is square and this frame is 2:3),
	// so the brand mark IS its poster. Only when there is no artwork at all: a plugin that does send
	// a cover has out-voted the token.
	const mark = !src && game.icon ? <LauncherIcon icon={game.icon} /> : null;

	return (
		<Card className="group relative overflow-hidden">
			<div className="relative aspect-[2/3] bg-muted">
				{/* Dim the ARTWORK only — never the badges or the buttons layered over it. A hidden
				    card is the sole place the title can be brought back, so its controls have to stay
				    at full contrast while the poster reads as "not in play". */}
				{src ? (
					<img
						src={src}
						alt={game.title}
						loading="lazy"
						className={`size-full object-cover${hidden ? " opacity-30" : ""}`}
						onError={() => setFailed((prev) => ({ ...prev, [src]: true }))}
					/>
				) : mark ? (
					// The mark carries the tile on its own — the launcher's name is already
					// directly below the frame, so repeating it here would just crowd the glyph.
					<div
						className={`flex size-full items-center justify-center p-8 text-muted-foreground${
							hidden ? " opacity-30" : ""
						}`}
					>
						<div className="w-full max-w-24 [&>svg]:size-full">{mark}</div>
					</div>
				) : (
					<div
						className={`flex size-full items-center justify-center p-3 text-center text-sm font-medium text-muted-foreground${
							hidden ? " opacity-30" : ""
						}`}
					>
						{game.title}
					</div>
				)}
				<div className="absolute left-2 top-2 flex flex-wrap gap-1">
					<Badge
						variant={isCustom ? "secondary" : "outline"}
						className="bg-background/80 backdrop-blur"
					>
						{storeLabel(game.store)}
					</Badge>
					{/* Platform badge — "PC" is implied by every installed store, so only
					    non-PC platforms (the emulation case) earn a second badge. */}
					{game.platform && game.platform.toUpperCase() !== "PC" && (
						<Badge variant="outline" className="bg-background/80 backdrop-blur">
							{game.platform}
						</Badge>
					)}
					{/* Who owns this entry, when it isn't the operator — the reason the edit/delete
					    buttons are absent here and present on the card next to it. */}
					{game.provider && (
						<Badge variant="outline" className="bg-background/80 backdrop-blur">
							{m.library_owned_by({ provider: game.provider })}
						</Badge>
					)}
					{/* Says WHY this poster is faded. Without it a dimmed tile reads as a broken cover
					    or a still-loading image rather than a deliberate setting. */}
					{hidden && (
						<Badge
							variant="secondary"
							className="bg-background/90 backdrop-blur"
						>
							{m.library_hidden_badge()}
						</Badge>
					)}
				</div>
				{/* A hidden card keeps its controls VISIBLE rather than hover-revealed. Hover-to-reveal
				    is fine for an ordinary tile, but the un-hide button is the only way out of the
				    hidden state — requiring a hover to discover it would strand anyone on a touch
				    screen, which is exactly where the console's pointer work landed.
				    Two rules that used to be one, because `opacity-0` alone got BOTH of them wrong:
				    1. Invisible must also mean UNCLICKABLE. An `opacity-0` element paints nothing and
				       still hit-tests, so the top-right corner of every poster was a live hide button
				       nobody could see — a stray click there dropped that title from every play
				       surface with no visible cause. Opacity and `pointer-events` move together now.
				    2. The reveal cannot rest on hover ALONE. `:hover` never fires on a touch screen,
				       so the control was unreachable there by construction and discoverable only by
				       blind-clicking the corner. `pointer-coarse` shows it outright wherever the
				       device has no hover to give. Keyboard reach is unaffected: `pointer-events:
				       none` does not block focus, so tabbing in still trips `focus-within`. */}
				<div
					className={`absolute right-2 top-2 flex gap-1 transition-opacity ${
						hidden
							? "opacity-100"
							: "opacity-0 pointer-events-none group-hover:opacity-100 group-hover:pointer-events-auto focus-within:opacity-100 focus-within:pointer-events-auto pointer-coarse:opacity-100 pointer-coarse:pointer-events-auto"
					}`}
				>
					<Button
						variant="secondary"
						size="icon"
						className="size-7 bg-background/80 backdrop-blur"
						aria-label={
							hidden ? m.library_unhide_action() : m.library_hide_action()
						}
						// A bare eye-with-slash is the only control on a SCANNED entry's card, with no
						// edit/delete beside it to imply a toolbar. The native tooltip is what tells a
						// pointer user what the icon does before they click and a game vanishes.
						title={hidden ? m.library_unhide_action() : m.library_hide_action()}
						aria-pressed={hidden}
						disabled={hiding}
						onClick={onToggleHidden}
					>
						{hidden ? (
							<Eye className="size-3.5" />
						) : (
							<EyeOff className="size-3.5" />
						)}
					</Button>
					{isCustom && (
						<>
							<Button
								variant="secondary"
								size="icon"
								className="size-7 bg-background/80 backdrop-blur"
								aria-label={m.library_edit()}
								onClick={onEdit}
							>
								<Pencil className="size-3.5" />
							</Button>
							<Button
								variant="secondary"
								size="icon"
								className="size-7 bg-background/80 backdrop-blur"
								aria-label={m.library_delete()}
								disabled={deleting}
								onClick={onDelete}
							>
								<Trash2 className="size-3.5 text-destructive" />
							</Button>
						</>
					)}
				</div>
			</div>
			<div
				className="truncate px-card pb-card pt-4 text-sm font-medium"
				title={game.title}
			>
				{game.title}
				{game.release_year != null && (
					<span className="ml-1.5 font-normal text-muted-foreground">
						{game.release_year}
					</span>
				)}
			</div>
		</Card>
	);
};
