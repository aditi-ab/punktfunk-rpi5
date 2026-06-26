import { Circle, Download, Eye, Square, Trash2, X } from "lucide-react";
import type { FC } from "react";
import type { Capture } from "@/api/gen/model/capture";
import type { CaptureMeta } from "@/api/gen/model/captureMeta";
import type { StatsStatus } from "@/api/gen/model/statsStatus";
import { QueryState } from "@/components/query-state";
import { Section } from "@/components/section";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
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
import { HealthChart, LatencyChart, ThroughputChart } from "./charts";

/** ms → `m:ss`. */
function fmtDuration(ms: number): string {
	const s = Math.max(0, Math.floor(ms / 1000));
	return `${Math.floor(s / 60)}:${(s % 60).toString().padStart(2, "0")}`;
}

function fmtTimestamp(unixMs: number): string {
	if (!unixMs) return "—";
	return new Date(unixMs).toLocaleString();
}

function kindLabel(kind: string): string {
	if (kind === "gamestream") return m.stats_kind_gamestream();
	if (kind === "native") return m.stats_kind_native();
	return kind;
}

export interface StatsViewProps {
	status: Loadable<StatsStatus>;
	live: Loadable<Capture>;
	recordings: Loadable<CaptureMeta[]>;
	detail: Loadable<Capture>;
	selectedId: string | null;
	onStart: () => void;
	onStop: () => void;
	onSelect: (id: string | null) => void;
	onDownload: (id: string) => void;
	onDelete: (id: string) => void;
	isStarting: boolean;
	isStopping: boolean;
	isDeleting: boolean;
}

export const StatsView: FC<StatsViewProps> = (props) => {
	const armed = props.status.data?.armed ?? false;
	return (
		<Section>
			<div className="space-y-1">
				<h1 className="text-2xl font-semibold">{m.stats_title()}</h1>
				<p className="text-sm text-muted-foreground">{m.stats_subtitle()}</p>
			</div>

			<CaptureControlCard
				status={props.status}
				onStart={props.onStart}
				onStop={props.onStop}
				isStarting={props.isStarting}
				isStopping={props.isStopping}
			/>

			{armed && <LiveCard live={props.live} />}

			<RecordingsCard
				recordings={props.recordings}
				selectedId={props.selectedId}
				onSelect={props.onSelect}
				onDownload={props.onDownload}
				onDelete={props.onDelete}
				isDeleting={props.isDeleting}
			/>

			{props.selectedId && (
				<DetailCard
					detail={props.detail}
					onClose={() => props.onSelect(null)}
				/>
			)}
		</Section>
	);
};

/** Start/Stop + a Recording/Idle pill with elapsed + sample count. */
const CaptureControlCard: FC<{
	status: Loadable<StatsStatus>;
	onStart: () => void;
	onStop: () => void;
	isStarting: boolean;
	isStopping: boolean;
}> = ({ status, onStart, onStop, isStarting, isStopping }) => {
	const s = status.data;
	const armed = s?.armed ?? false;
	const elapsed = armed && s ? Date.now() - s.started_unix_ms : 0;
	return (
		<QueryState
			isLoading={status.isLoading}
			error={status.error}
			refetch={status.refetch}
		>
			<Card>
				<CardHeader>
					<CardTitle className="flex items-center justify-between gap-3">
						<span>{m.stats_capture_title()}</span>
						{armed ? (
							<Badge variant="destructive" className="gap-1.5">
								<Circle className="size-2.5 animate-pulse fill-current" />
								{m.stats_recording()}
							</Badge>
						) : (
							<Badge variant="outline">{m.stats_idle()}</Badge>
						)}
					</CardTitle>
				</CardHeader>
				<CardContent className="space-y-4">
					<p className="text-sm text-muted-foreground">
						{m.stats_capture_desc()}
					</p>
					{armed && s && (
						<dl className="flex flex-wrap gap-x-8 gap-y-2 text-sm tabular-nums">
							<Stat label={m.stats_elapsed()} value={fmtDuration(elapsed)} />
							<Stat label={m.stats_samples()} value={String(s.sample_count)} />
							{s.kind && (
								<Stat label={m.stats_kind()} value={kindLabel(s.kind)} />
							)}
						</dl>
					)}
					<div className="flex gap-2">
						{armed ? (
							<Button
								variant="destructive"
								disabled={isStopping}
								onClick={onStop}
							>
								<Square className="size-4" />
								{m.stats_stop()}
							</Button>
						) : (
							<Button disabled={isStarting} onClick={onStart}>
								<Circle className="size-4 fill-current" />
								{m.stats_start()}
							</Button>
						)}
					</div>
				</CardContent>
			</Card>
		</QueryState>
	);
};

const Stat: FC<{ label: string; value: string }> = ({ label, value }) => (
	<div className="flex flex-col">
		<dt className="text-xs text-muted-foreground">{label}</dt>
		<dd className="font-medium">{value}</dd>
	</div>
);

/** Live graphs while a capture is armed: latency stack + throughput. */
const LiveCard: FC<{ live: Loadable<Capture> }> = ({ live }) => {
	const samples = live.data?.samples ?? [];
	return (
		<Card>
			<CardHeader>
				<CardTitle>{m.stats_live_title()}</CardTitle>
			</CardHeader>
			<CardContent className="space-y-8">
				{samples.length === 0 ? (
					<p className="py-8 text-center text-sm text-muted-foreground">
						{m.stats_live_waiting()}
					</p>
				) : (
					<>
						<ChartBlock
							title={m.stats_latency_title()}
							desc={m.stats_latency_desc()}
						>
							<LatencyChart samples={samples} />
						</ChartBlock>
						<ChartBlock title={m.stats_throughput_title()}>
							<ThroughputChart samples={samples} />
						</ChartBlock>
					</>
				)}
			</CardContent>
		</Card>
	);
};

/** Saved recordings, with View / Download / Delete row actions. */
const RecordingsCard: FC<{
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
		<div className="space-y-2">
			<h2 className="text-lg font-medium">{m.stats_recordings_title()}</h2>
			<QueryState
				isLoading={recordings.isLoading}
				error={recordings.error}
				refetch={recordings.refetch}
			>
				{rows.length === 0 ? (
					<Card>
						<CardContent className="p-8 text-center text-sm text-muted-foreground">
							{m.stats_recordings_empty()}
						</CardContent>
					</Card>
				) : (
					<Card>
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
					</Card>
				)}
			</QueryState>
		</div>
	);
};

/** Full graph set for one selected recording: latency (p99 toggle) + throughput + health. */
const DetailCard: FC<{ detail: Loadable<Capture>; onClose: () => void }> = ({
	detail,
	onClose,
}) => {
	const cap = detail.data;
	const samples = cap?.samples ?? [];
	return (
		<Card>
			<CardHeader>
				<CardTitle className="flex items-center justify-between gap-3">
					<span>
						{m.stats_detail_title()}
						{cap && (
							<span className="ml-2 text-sm font-normal text-muted-foreground">
								{cap.meta.width}×{cap.meta.height}@{cap.meta.fps} ·{" "}
								{cap.meta.codec.toUpperCase()}
							</span>
						)}
					</span>
					<Button
						variant="ghost"
						size="icon"
						aria-label={m.stats_close()}
						onClick={onClose}
					>
						<X className="size-4" />
					</Button>
				</CardTitle>
			</CardHeader>
			<CardContent>
				<QueryState
					isLoading={detail.isLoading}
					error={detail.error}
					refetch={detail.refetch}
				>
					{samples.length === 0 ? (
						<p className="py-8 text-center text-sm text-muted-foreground">
							{m.stats_no_samples()}
						</p>
					) : (
						<div className="space-y-8">
							<ChartBlock
								title={m.stats_latency_title()}
								desc={m.stats_latency_desc()}
							>
								<LatencyChart samples={samples} toggle />
							</ChartBlock>
							<ChartBlock title={m.stats_throughput_title()}>
								<ThroughputChart samples={samples} />
							</ChartBlock>
							<ChartBlock title={m.stats_health_title()}>
								<HealthChart samples={samples} />
							</ChartBlock>
						</div>
					)}
				</QueryState>
			</CardContent>
		</Card>
	);
};

const ChartBlock: FC<{
	title: string;
	desc?: string;
	children: React.ReactNode;
}> = ({ title, desc, children }) => (
	<div className="space-y-2">
		<div>
			<h3 className="text-sm font-medium">{title}</h3>
			{desc && <p className="text-xs text-muted-foreground">{desc}</p>}
		</div>
		{children}
	</div>
);
