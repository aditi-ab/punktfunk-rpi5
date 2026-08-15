import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { Download, Trash2 } from "lucide-react";
import type { FC } from "react";
import type { ClientLogMeta } from "@/api/gen/model/clientLogMeta";
import {
	clientLogsGet,
	getClientLogsListQueryKey,
	useClientLogsDelete,
	useClientLogsList,
} from "@/api/gen/logs/logs";
import { useDialogs } from "@/components/dialogs";
import { QueryState } from "@/components/query-state";
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
import { apiErrorMessage } from "@/lib/errors";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import { fmtTimestamp } from "../Stats/helpers";

/** `123.4 KB` — bundles are ≤1 MiB, so two units cover the whole range. */
const fmtSize = (bytes: number): string =>
	bytes >= 1024 * 1024
		? `${(bytes / (1024 * 1024)).toFixed(1)} MB`
		: `${(bytes / 1024).toFixed(1)} KB`;

/**
 * Container: log bundles paired clients uploaded via "Send logs to host" — the log-escape hatch
 * for platforms whose own files the user can't reach (a Deck in Gaming Mode, tvOS). Owns the
 * list query, the text download, and delete.
 */
export const ClientLogsSection: FC = () => {
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	const bundles = useClientLogsList();
	const del = useClientLogsDelete();

	const onDelete = async (id: string) => {
		const ok = await confirm({
			title: m.client_logs_delete_confirm(),
			description: m.client_logs_delete_body(),
			confirmLabel: m.client_logs_delete(),
			destructive: true,
		});
		if (!ok) return;
		del.mutate(
			{ id },
			{
				onSuccess: () =>
					qc.invalidateQueries({ queryKey: getClientLogsListQueryKey() }),
				onError: (e) =>
					toast.error(apiErrorMessage(e) ?? m.client_logs_delete_failed()),
			},
		);
	};

	// Plain-text bundle → blob download, same shape as the recordings JSON export.
	const onDownload = async (id: string) => {
		try {
			const text = await clientLogsGet(id);
			const blob = new Blob([text], { type: "text/plain" });
			const url = URL.createObjectURL(blob);
			const a = document.createElement("a");
			a.href = url;
			a.download = `${id}.log`;
			document.body.appendChild(a);
			a.click();
			a.remove();
			URL.revokeObjectURL(url);
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.client_logs_download_failed());
		}
	};

	return (
		<ClientLogsCard
			bundles={bundles}
			onDownload={onDownload}
			onDelete={onDelete}
			isDeleting={del.isPending}
		/>
	);
};

/** Uploaded client log bundles, newest first, with Download / Delete row actions. */
export const ClientLogsCard: FC<{
	bundles: Loadable<ClientLogMeta[]>;
	onDownload: (id: string) => void;
	onDelete: (id: string) => void;
	isDeleting: boolean;
}> = ({ bundles, onDownload, onDelete, isDeleting }) => {
	const rows = bundles.data ?? [];
	// No bundles is the ordinary state (nothing was ever sent) — an empty card would just be
	// noise on every visit, so the whole card only appears once something arrived. Errors and
	// loading still render: a broken list must not look like "nothing was sent".
	if (!bundles.isLoading && !bundles.error && rows.length === 0) return null;
	return (
		<Card>
			<CardHeader>
				<div className="space-y-1">
					<h2 className="text-lg font-medium">{m.client_logs_title()}</h2>
					<p className="text-sm text-muted-foreground">
						{m.client_logs_subtitle()}
					</p>
				</div>
			</CardHeader>
			<QueryState
				isLoading={bundles.isLoading}
				error={bundles.error}
				refetch={bundles.refetch}
			>
				<CardContent flush>
					<Table>
						<TableHeader>
							<TableRow>
								<TableHead>{m.client_logs_col_received()}</TableHead>
								<TableHead>{m.client_logs_col_device()}</TableHead>
								<TableHead className="text-right">
									{m.client_logs_col_size()}
								</TableHead>
								<TableHead className="w-24" />
							</TableRow>
						</TableHeader>
						<TableBody>
							{rows.map((r) => (
								<TableRow key={r.id}>
									<TableCell className="whitespace-nowrap font-medium">
										{fmtTimestamp(r.received_ms)}
									</TableCell>
									<TableCell>
										<span>{r.device_name}</span>
										<span className="ml-2 font-mono text-xs text-muted-foreground">
											{r.fingerprint_prefix}
										</span>
									</TableCell>
									<TableCell className="text-right tabular-nums">
										{fmtSize(r.size_bytes)}
									</TableCell>
									<TableCell>
										<div className="flex justify-end gap-1">
											<Button
												variant="ghost"
												size="icon"
												aria-label={m.client_logs_download()}
												title={m.client_logs_download()}
												onClick={() => onDownload(r.id)}
											>
												<Download className="size-4" />
											</Button>
											<Button
												variant="ghost"
												size="icon"
												aria-label={m.client_logs_delete()}
												title={m.client_logs_delete()}
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
			</QueryState>
		</Card>
	);
};
