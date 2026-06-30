import { useQueryClient } from "@tanstack/react-query";
import { Download, Eye, Trash2 } from "lucide-react";
import type { FC } from "react";
import type { CaptureMeta } from "@/api/gen/model/captureMeta";
import {
	getStatsRecordingsListQueryKey,
	statsRecordingGet,
	useStatsRecordingDelete,
	useStatsRecordingsList,
} from "@/api/gen/stats/stats";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import { fmtDuration, fmtTimestamp, kindLabel } from "./helpers";

/**
 * Container: the saved recordings. Owns the list query, delete, and the JSON export. Selection is
 * the parent's UI state (it also drives the detail card), passed through here for row highlight +
 * to clear it when the selected recording is deleted.
 */
export const RecordingsSection: FC<{
	selectedId: string | null;
	onSelect: (id: string | null) => void;
}> = ({ selectedId, onSelect }) => {
	const qc = useQueryClient();
	const recordings = useStatsRecordingsList();
	const del = useStatsRecordingDelete();

	const onDelete = (id: string) => {
		if (!confirm(m.stats_delete_confirm())) return;
		del.mutate(
			{ id },
			{
				onSuccess: () => {
					if (selectedId === id) onSelect(null);
					qc.invalidateQueries({ queryKey: getStatsRecordingsListQueryKey() });
				},
			},
		);
	};

	// Export the full Capture JSON via a one-off GET → blob download.
	const onDownload = async (id: string) => {
		try {
			const cap = await statsRecordingGet(id);
			const blob = new Blob([JSON.stringify(cap, null, 2)], {
				type: "application/json",
			});
			const url = URL.createObjectURL(blob);
			const a = document.createElement("a");
			a.href = url;
			a.download = `${id}.json`;
			document.body.appendChild(a);
			a.click();
			a.remove();
			URL.revokeObjectURL(url);
		} catch {
			// Best-effort export; the recording GET surfaces its own errors via the detail view.
		}
	};

	return (
		<RecordingsCard
			recordings={recordings}
			selectedId={selectedId}
			onSelect={onSelect}
			onDownload={onDownload}
			onDelete={onDelete}
			isDeleting={del.isPending}
		/>
	);
};

/** Saved recordings, with View / Download / Delete row actions. */
export const RecordingsCard: FC<{
	recordings: Loadable<CaptureMeta[]>;
	selectedId: string | null;
	onSelect: (id: string | null) => void;
	onDownload: (id: string) => void;
	onDelete: (id: string) => void;
	isDeleting: boolean;
}> = ({
	recordings,
	selectedId,
	onSelect,
	onDownload,
	onDelete,
	isDeleting,
}) => {
	const rows = recordings.data ?? [];
	return (
		<Card>
			<CardHeader>
				<h2 className="text-lg font-medium">{m.stats_recordings_title()}</h2>
			</CardHeader>
			<QueryState
				isLoading={recordings.isLoading}
				error={recordings.error}
				refetch={recordings.refetch}
			>
				{rows.length === 0 ? (
					<CardContent className="p-8 text-center text-sm text-muted-foreground">
						{m.stats_recordings_empty()}
					</CardContent>
				) : (
					<CardContent className="p-0">
						<Table>
							<TableHeader>
								<TableRow>
									<TableHead>{m.stats_col_time()}</TableHead>
									<TableHead>{m.stats_col_kind()}</TableHead>
									<TableHead>{m.stats_col_resolution()}</TableHead>
									<TableHead>{m.stats_col_codec()}</TableHead>
									<TableHead className="text-right">
										{m.stats_col_duration()}
									</TableHead>
									<TableHead className="text-right">
										{m.stats_col_samples()}
									</TableHead>
									<TableHead className="w-32" />
								</TableRow>
							</TableHeader>
							<TableBody>
								{rows.map((r) => (
									<TableRow
										key={r.id}
										data-state={selectedId === r.id ? "selected" : undefined}
									>
										<TableCell className="whitespace-nowrap font-medium">
											{fmtTimestamp(r.started_unix_ms)}
										</TableCell>
										<TableCell>
											<Badge
												variant={
													r.kind === "gamestream" ? "secondary" : "default"
												}
											>
												{kindLabel(r.kind)}
											</Badge>
										</TableCell>
										<TableCell className="tabular-nums text-muted-foreground">
											{r.width}×{r.height}@{r.fps}
										</TableCell>
										<TableCell className="uppercase text-muted-foreground">
											{r.codec}
										</TableCell>
										<TableCell className="text-right tabular-nums">
											{fmtDuration(r.duration_ms)}
										</TableCell>
										<TableCell className="text-right tabular-nums">
											{r.sample_count}
										</TableCell>
										<TableCell>
											<div className="flex justify-end gap-1">
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.stats_view()}
													title={m.stats_view()}
													onClick={() =>
														onSelect(selectedId === r.id ? null : r.id)
													}
												>
													<Eye className="size-4" />
												</Button>
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.stats_download()}
													title={m.stats_download()}
													onClick={() => onDownload(r.id)}
												>
													<Download className="size-4" />
												</Button>
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.stats_delete()}
													title={m.stats_delete()}
													disabled={isDeleting}
													onClick={() => onDelete(r.id)}
												>
													<Trash2 className="size-4 text-destructive" />
												</Button>
											</div>
										</TableCell>
									</TableRow>
								))}
							</TableBody>
						</Table>
					</CardContent>
				)}
			</QueryState>
		</Card>
	);
};
