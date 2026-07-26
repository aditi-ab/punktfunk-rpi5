import { Pencil, Trash2 } from "lucide-react";
import { type FC, useState } from "react";
import type { GameEntry } from "@/api/gen/model/gameEntry";
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
	game: GameEntry;
	onEdit: () => void;
	onDelete: () => void;
	deleting: boolean;
}

/**
 * A poster tile. The cover prefers the 2:3 portrait capsule; on a load error it
 * falls back to the wide header, then to a text placeholder. Custom entries get
 * edit/delete affordances.
 */
export const GameCard: FC<GameCardProps> = ({
	game,
	onEdit,
	onDelete,
	deleting,
}) => {
	const isCustom = game.store === "custom";
	// Track which sources have failed so the <img> can step down portrait → header → placeholder.
	const [failed, setFailed] = useState<Record<string, boolean>>({});

	const candidates = [game.art.portrait, game.art.header].filter(
		(u): u is string => !!u && !failed[u],
	);
	const src = candidates[0];

	return (
		<Card className="group relative overflow-hidden">
			<div className="relative aspect-[2/3] bg-muted">
				{src ? (
					<img
						src={src}
						alt={game.title}
						loading="lazy"
						className="size-full object-cover"
						onError={() => setFailed((prev) => ({ ...prev, [src]: true }))}
					/>
				) : (
					<div className="flex size-full items-center justify-center p-3 text-center text-sm font-medium text-muted-foreground">
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
				</div>
				{isCustom && (
					<div className="absolute right-2 top-2 flex gap-1 opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100">
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
					</div>
				)}
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
