import type { FC } from "react";
import type { Capture } from "@/api/gen/model/capture";
import {
	useStatsCaptureLive,
	useStatsCaptureStatus,
} from "@/api/gen/stats/stats";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import { LatencyChart, ThroughputChart } from "./charts";
import { ChartBlock } from "./helpers";

/**
 * Container: the live graphs. Self-gates on the capture being armed — it shares the status query
 * (same key) with the control card, and only fetches the in-progress capture while armed (it 404s
 * when idle). Renders nothing when no capture is running.
 */
export const LiveSection: FC = () => {
	const status = useStatsCaptureStatus({ query: { refetchInterval: 2_000 } });
	const armed = status.data?.armed ?? false;
	const live = useStatsCaptureLive({
		query: { refetchInterval: 2_000, enabled: armed },
	});
	if (!armed) return null;
	return <LiveCard live={live} />;
};

/** Live graphs while a capture is armed: latency stack + throughput. */
export const LiveCard: FC<{ live: Loadable<Capture> }> = ({ live }) => {
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
