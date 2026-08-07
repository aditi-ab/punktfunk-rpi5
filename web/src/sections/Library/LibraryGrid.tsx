import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { motion, stagger } from "motion/react";
import { type FC, useEffect, useMemo } from "react";
import {
	getGetLibraryQueryKey,
	useDeleteCustomGame,
	useGetLibrary,
} from "@/api/gen/library/library";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import { QueryState } from "@/components/query-state";
import { Card, CardContent } from "@/components/ui/card";
import { apiErrorMessage } from "@/lib/errors";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import { GameCard } from "./GameCard";
import { customId } from "./helpers";

/**
 * Container: the library OVERVIEW — owns the listing query and per-card delete.
 * Editing is escalated to the parent (it opens the separate add/edit form), so
 * this subsection knows nothing about the form beyond firing `onEdit`.
 */
export const LibraryGridSection: FC<{
	onEdit: (entry: GameEntry) => void;
	/** Show only entries owned by this provider, or everything when null. */
	providerFilter?: string | null;
	/** Reports the full (unfiltered) list up, so the providers card can count owners. */
	onEntries?: (entries: GameEntry[]) => void;
}> = ({ onEdit, providerFilter, onEntries }) => {
	const qc = useQueryClient();
	const library = useGetLibrary();
	const all = library.data;
	useEffect(() => {
		if (all) onEntries?.(all);
	}, [all, onEntries]);
	// Filtering CLIENT-side: `GET /library?provider=` exists, but the page already holds the whole
	// list for the grid, and a second parameterised query would just be a second cache entry of the
	// same data going stale independently.
	const filtered = useMemo(
		() =>
			providerFilter
				? {
						...library,
						data: all?.filter((e) => e.provider === providerFilter),
					}
				: library,
		[library, all, providerFilter],
	);
	const remove = useDeleteCustomGame();

	// A refused delete has to say so. The host has real reasons to say no (a provider-owned entry
	// answers 409 with what to do instead), and an un-caught `mutateAsync` rejection reported none
	// of them — the card just stayed put as if nothing had been clicked.
	const onDelete = async (entry: GameEntry) => {
		if (!confirm(m.library_delete_confirm())) return;
		try {
			await remove.mutateAsync({ id: customId(entry) });
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_delete_failed());
			return;
		}
		qc.invalidateQueries({ queryKey: getGetLibraryQueryKey() });
	};

	return (
		<LibraryGrid
			library={filtered}
			onEdit={onEdit}
			onDelete={onDelete}
			// The custom id whose delete is in flight (if any), so only that card's button disables.
			deletingId={remove.isPending ? (remove.variables?.id ?? null) : null}
		/>
	);
};

/** The poster grid (with empty + loading/error states). */
export const LibraryGrid: FC<{
	library: Loadable<GameEntry[]>;
	onEdit: (entry: GameEntry) => void;
	onDelete: (entry: GameEntry) => void;
	/** Custom id of the card whose delete is in flight, or null — only that card disables. */
	deletingId: string | null;
}> = ({ library, onEdit, onDelete, deletingId }) => {
	const all = library.data ?? [];
	// Launcher entries (design D4) open the launcher itself — Steam Big Picture, Heroic — rather than
	// a title. They launch and lease exactly like games; grouping them into their own rail is purely
	// so a shelf of 400 games doesn't bury the two or three ways to open a launcher.
	const launchers = all.filter((g) => g.role === "launcher");
	const games = all.filter((g) => g.role !== "launcher");
	const card = (game: GameEntry) => (
		<GameCard
			key={game.id}
			game={game}
			onEdit={() => onEdit(game)}
			onDelete={() => onDelete(game)}
			deleting={deletingId === customId(game)}
		/>
	);
	return (
		<QueryState
			isLoading={library.isLoading}
			error={library.error}
			refetch={library.refetch}
		>
			{launchers.length > 0 && (
				<div className="@container mb-card">
					<p className="pb-2 text-xs font-medium uppercase tracking-wide text-muted-foreground/70">
						{m.library_launchers_title()}
					</p>
					<motion.div
						transition={{ delayChildren: stagger(0.1) }}
						variants={{ enter: {}, from: {} }}
						className="grid grid-cols-1 gap-card @sm:grid-cols-2 @md:grid-cols-2 @lg:grid-cols-3 @2xl:grid-cols-4 @4xl:grid-cols-5"
					>
						{launchers.map(card)}
					</motion.div>
				</div>
			)}
			{all.length === 0 ? (
				<Card>
					{/* `flush`, not a bare `p-8`: the default `sm:pt-0` would survive the override
					    (tailwind-merge only resolves conflicts within a variant) and eat the top
					    inset at ≥640px — see the CardContent doc comment. */}
					<CardContent
						flush
						className="p-8 text-center text-sm text-muted-foreground"
					>
						{/* After extraction a fresh host has NO scanners at all, so "no games" is the
						    expected first-run state rather than a fault. Point at the fix (design D9)
						    instead of leaving a bare empty grid. */}
						<p>{m.library_empty()}</p>
						<p className="mt-2">{m.library_empty_add_source()}</p>
					</CardContent>
				</Card>
			) : (
				games.length > 0 && (
					<div className="@container">
						<motion.div
							transition={{ delayChildren: stagger(0.1) }}
							variants={{ enter: {}, from: {} }}
							className="grid grid-cols-1 gap-card @sm:grid-cols-2 @md:grid-cols-2 @lg:grid-cols-3 @2xl:grid-cols-4 @4xl:grid-cols-5"
						>
							{games.map(card)}
						</motion.div>
					</div>
				)
			)}
		</QueryState>
	);
};
