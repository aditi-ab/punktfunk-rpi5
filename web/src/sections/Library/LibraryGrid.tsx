import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { type FC, useEffect, useMemo } from "react";
import {
	getGetLibraryQueryKey,
	useDeleteCustomGame,
	useGetLibrary,
	useSetLibraryEntryHidden,
} from "@/api/gen/library/library";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { useDialogs } from "@/components/dialogs";
import { QueryState } from "@/components/query-state";
import { Stagger } from "@/components/stagger";
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
	onEdit: (entry: OperatorGameEntry) => void;
	/** Show only entries owned by this provider, or everything when null. */
	providerFilter?: string | null;
	/** Reports the full (unfiltered) list up, so the providers card can count owners. */
	onEntries?: (entries: OperatorGameEntry[]) => void;
}> = ({ onEdit, providerFilter, onEntries }) => {
	const qc = useQueryClient();
	const { confirm } = useDialogs();
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
	const onDelete = async (entry: OperatorGameEntry) => {
		const ok = await confirm({
			title: m.library_delete_confirm(),
			description: m.library_delete_body(),
			confirmLabel: m.library_delete(),
			destructive: true,
		});
		if (!ok) return;
		try {
			await remove.mutateAsync({ id: customId(entry) });
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_delete_failed());
			return;
		}
		qc.invalidateQueries({ queryKey: getGetLibraryQueryKey() });
	};

	const setHidden = useSetLibraryEntryHidden();

	// Same error discipline as delete: the host can refuse (it cannot persist the settings file),
	// and swallowing that would leave the card looking unchanged with no explanation.
	const onToggleHidden = async (entry: OperatorGameEntry) => {
		try {
			await setHidden.mutateAsync({
				id: entry.id,
				data: { hidden: entry.hidden !== true },
			});
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_hide_failed());
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
			onToggleHidden={onToggleHidden}
			// Keyed by ENTRY id, not custom id — hiding addresses any store's entry, not just ours.
			hidingId={setHidden.isPending ? (setHidden.variables?.id ?? null) : null}
		/>
	);
};

/** The poster grid (with empty + loading/error states). */
export const LibraryGrid: FC<{
	library: Loadable<OperatorGameEntry[]>;
	onEdit: (entry: OperatorGameEntry) => void;
	onDelete: (entry: OperatorGameEntry) => void;
	/** Custom id of the card whose delete is in flight, or null — only that card disables. */
	deletingId: string | null;
	onToggleHidden: (entry: OperatorGameEntry) => void;
	/** Entry id of the card whose hide/un-hide is in flight, or null. */
	hidingId: string | null;
}> = ({ library, onEdit, onDelete, deletingId, onToggleHidden, hidingId }) => {
	const all = library.data ?? [];
	// Launcher entries (design D4) open the launcher itself — Steam Big Picture, Heroic — rather than
	// a title. They launch and lease exactly like games; grouping them into their own rail is purely
	// so a shelf of 400 games doesn't bury the two or three ways to open a launcher.
	const launchers = all.filter((g) => g.role === "launcher");
	const games = all.filter((g) => g.role !== "launcher");
	const card = (game: OperatorGameEntry) => (
		<GameCard
			key={game.id}
			game={game}
			onEdit={() => onEdit(game)}
			onDelete={() => onDelete(game)}
			deleting={deletingId === customId(game)}
			onToggleHidden={() => onToggleHidden(game)}
			hiding={hidingId === game.id}
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
					<Stagger className="grid grid-cols-1 gap-card @sm:grid-cols-2 @md:grid-cols-2 @lg:grid-cols-3 @2xl:grid-cols-4 @4xl:grid-cols-5">
						{launchers.map(card)}
					</Stagger>
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
						<Stagger className="grid grid-cols-1 gap-card @sm:grid-cols-2 @md:grid-cols-2 @lg:grid-cols-3 @2xl:grid-cols-4 @4xl:grid-cols-5">
							{games.map(card)}
						</Stagger>
					</div>
				)
			)}
		</QueryState>
	);
};
