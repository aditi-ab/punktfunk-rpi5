import { X } from "lucide-react";
import type { FC } from "react";
import type { Capture } from "@/api/gen/model/capture";
import { useStatsRecordingGet } from "@/api/gen/stats/stats";
import { QueryState } from "@/components/query-state";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import { HealthChart, LatencyChart, ThroughputChart } from "./charts";
import { ChartBlock } from "./helpers";

/** Container: the full graph set for the selected recording — fetched by id. */
export const DetailSection: FC<{ id: string; onClose: () => void }> = ({
	id,
	onClose,
}) => {
	const detail = useStatsRecordingGet(id, { query: { enabled: !!id } });
	return <DetailCard detail={detail} onClose={onClose} />;
};

/** Full graph set for one selected recording: latency (p99 toggle) + throughput + health. */
export const DetailCard: FC<{
	detail: Loadable<Capture>;
	onClose: () => void;
}> = ({ detail, onClose }) => {
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
