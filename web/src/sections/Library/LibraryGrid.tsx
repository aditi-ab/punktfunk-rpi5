import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { motion, stagger } from "motion/react";
import type { FC } from "react";
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
export const LibraryGridSection: FC<{ onEdit: (entry: GameEntry) => void }> = ({
	onEdit,
}) => {
	const qc = useQueryClient();
	const library = useGetLibrary();
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
			library={library}
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
	const games = library.data ?? [];
	return (
		<QueryState
			isLoading={library.isLoading}
			error={library.error}
			refetch={library.refetch}
		>
			{games.length === 0 ? (
				<Card>
					{/* `flush`, not a bare `p-8`: the default `sm:pt-0` would survive the override
					    (tailwind-merge only resolves conflicts within a variant) and eat the top
					    inset at ≥640px — see the CardContent doc comment. */}
					<CardContent
						flush
						className="p-8 text-center text-sm text-muted-foreground"
					>
						{m.library_empty()}
					</CardContent>
				</Card>
			) : (
				<div className="@container">
					<motion.div
						transition={{ delayChildren: stagger(0.1) }}
						variants={{ enter: {}, from: {} }}
						className="grid grid-cols-1 gap-card @sm:grid-cols-2 @md:grid-cols-2 @lg:grid-cols-3 @2xl:grid-cols-4 @4xl:grid-cols-5"
					>
						{games.map((game) => (
							<GameCard
								key={game.id}
								game={game}
								onEdit={() => onEdit(game)}
								onDelete={() => onDelete(game)}
								deleting={deletingId === customId(game)}
							/>
						))}
					</motion.div>
				</div>
			)}
		</QueryState>
	);
};
