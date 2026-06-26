import { useQueryClient } from "@tanstack/react-query";
import { type FC, useState } from "react";
import {
	getStatsCaptureStatusQueryKey,
	getStatsRecordingsListQueryKey,
	statsRecordingGet,
	useStatsCaptureLive,
	useStatsCaptureStart,
	useStatsCaptureStatus,
	useStatsCaptureStop,
	useStatsRecordingDelete,
	useStatsRecordingGet,
	useStatsRecordingsList,
} from "@/api/gen/stats/stats";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { StatsView } from "./view";

export const SectionStats: FC = () => {
	useLocale();
	const qc = useQueryClient();
	const [selectedId, setSelectedId] = useState<string | null>(null);

	// Poll the capture status (drives the control card + whether the live chart shows).
	const status = useStatsCaptureStatus({ query: { refetchInterval: 2_000 } });
	const armed = status.data?.armed ?? false;

	// Live in-progress capture — only fetched while armed (404s when idle).
	const live = useStatsCaptureLive({
		query: { refetchInterval: 2_000, enabled: armed },
	});

	const recordings = useStatsRecordingsList();

	// Selected recording detail — only fetched once a row is chosen.
	const detail = useStatsRecordingGet(selectedId ?? "", {
		query: { enabled: !!selectedId },
	});

	const start = useStatsCaptureStart();
	const stop = useStatsCaptureStop();
	const del = useStatsRecordingDelete();

	const refreshStatus = () =>
		qc.invalidateQueries({ queryKey: getStatsCaptureStatusQueryKey() });
	const refreshRecordings = () =>
		qc.invalidateQueries({ queryKey: getStatsRecordingsListQueryKey() });

	const onStart = () => start.mutate(undefined, { onSuccess: refreshStatus });
	const onStop = () =>
		stop.mutate(undefined, {
			onSuccess: () => {
				refreshStatus();
				refreshRecordings();
			},
		});

	const onDelete = (id: string) => {
		if (!confirm(m.stats_delete_confirm())) return;
		del.mutate(
			{ id },
			{
				onSuccess: () => {
					if (selectedId === id) setSelectedId(null);
					refreshRecordings();
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
		<StatsView
			status={status}
			live={live}
			recordings={recordings}
			detail={detail}
			selectedId={selectedId}
			onStart={onStart}
			onStop={onStop}
			onSelect={setSelectedId}
			onDownload={onDownload}
			onDelete={onDelete}
			isStarting={start.isPending}
			isStopping={stop.isPending}
			isDeleting={del.isPending}
		/>
	);
};
