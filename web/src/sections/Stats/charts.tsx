// Recharts visualisations for a captured stats series. Everything here is rendered
// CLIENT-ONLY (behind <ChartFrame>'s mounted guard): recharts' ResponsiveContainer
// measures its parent via ResizeObserver, which has no width during SSR and would
// otherwise render a 0×0 (or warn). The charts adapt to whatever stages a sample
// carries — native (queue/capture/submit/encode/send) and gamestream
// (capture/encode/packetize/send) both stack sensibly.
import { type ReactElement, useEffect, useMemo, useState } from "react";
import {
	Area,
	AreaChart,
	CartesianGrid,
	Legend,
	Line,
	LineChart,
	ResponsiveContainer,
	Tooltip,
	XAxis,
	YAxis,
} from "recharts";
import type { StatsSample } from "@/api/gen/model/statsSample";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages";

const CHART_H = 240;

const axisTick = { fontSize: 11, fill: "var(--muted-foreground)" } as const;
const gridStroke = "var(--border)";
const tooltipStyle = {
	background: "var(--card)",
	border: "1px solid var(--border)",
	borderRadius: 8,
	fontSize: 12,
	color: "var(--foreground)",
} as const;
const legendStyle = { fontSize: 12 } as const;

// Known stages get a stable hue; anything else falls back to the palette by
// appearance order, so an unexpected stage name still renders a distinct band.
const STAGE_COLORS: Record<string, string> = {
	// `queue` (native pipeline's first stage) needs its own hue — without it, it fell back to
	// PALETTE[0], which is capture's exact purple, making the bottom two bands indistinguishable.
	queue: "#64748b",
	capture: "#6c5bf3",
	submit: "#22a2f2",
	encode: "#f2a922",
	packetize: "#1fb6a8",
	send: "#f25c8a",
};
const PALETTE = [
	"#6c5bf3",
	"#22a2f2",
	"#f2a922",
	"#1fb6a8",
	"#f25c8a",
	"#9b6cf3",
];

/** True only after the first client-side effect — gates recharts off the server render. */
function useMounted(): boolean {
	const [mounted, setMounted] = useState(false);
	useEffect(() => setMounted(true), []);
	return mounted;
}

/** Reserves the chart's height during SSR / before mount, then swaps in the responsive chart. */
function ChartFrame({ children }: { children: ReactElement }) {
	const mounted = useMounted();
	if (!mounted) return <div style={{ height: CHART_H }} aria-hidden />;
	return (
		<ResponsiveContainer width="100%" height={CHART_H}>
			{children}
		</ResponsiveContainer>
	);
}

/** Stage names across all samples, in first-seen (pipeline) order. */
function stageNames(samples: StatsSample[]): string[] {
	const seen: string[] = [];
	for (const s of samples)
		for (const st of s.stages) if (!seen.includes(st.name)) seen.push(st.name);
	return seen;
}

function colorFor(name: string, i: number): string {
	return STAGE_COLORS[name] ?? PALETTE[i % PALETTE.length] ?? "#6c5bf3";
}

/**
 * Shared X-axis config for every chart here.
 *
 * `type="number"` + an explicit domain, NOT recharts' default category axis. As a category axis
 * every sample is one evenly-spaced slot, so a capture that idled for two minutes drew that gap as
 * a single step and the timeline was a lie — precisely the thing you are reading these charts to
 * find. As a number axis the spacing is the actual elapsed time.
 */
const timeAxis = {
	dataKey: "t",
	type: "number",
	domain: ["dataMin", "dataMax"],
	scale: "time",
	tick: axisTick,
	stroke: gridStroke,
	unit: "s",
	allowDecimals: false,
} as const;

/**
 * Split a capture at every session boundary and insert a gap between the pieces.
 *
 * A capture can span more than one session (`StatsSample.session_id`), and joining those samples
 * into one continuous line implies a continuity that never existed — the stream stopped and a
 * different client started a new one. Recharts breaks a line wherever a value is `null`, so one
 * spacer row between sessions renders the discontinuity without any per-chart special-casing.
 */
function withSessionBreaks<T extends { t: number }>(
	samples: StatsSample[],
	rows: T[],
): (T | { t: number })[] {
	const out: (T | { t: number })[] = [];
	for (let i = 0; i < rows.length; i++) {
		const row = rows[i];
		if (!row) continue;
		const prev = samples[i - 1];
		const cur = samples[i];
		if (prev && cur && prev.session_id !== cur.session_id) {
			// A bare `t` row: every series key is absent ⇒ null ⇒ recharts lifts the pen.
			out.push({ t: row.t - 0.001 });
		}
		out.push(row);
	}
	return out;
}

/** Seconds since the capture began, as a number (see `timeAxis`). */
const tSeconds = (s: StatsSample): number => s.t_ms / 1000;

/** Latency stacked-area (µs) — the "where does the time go" view. With `toggle`, a
 *  p50/p99 switch flips every stage band between its median and tail. */
export function LatencyChart({
	samples,
	toggle,
}: {
	samples: StatsSample[];
	toggle?: boolean;
}) {
	const [p99, setP99] = useState(false);
	const names = useMemo(() => stageNames(samples), [samples]);
	// Memoised: this walks every sample × every stage, and the live card re-renders it on a 2 s
	// poll. Without this the whole series was rebuilt on every unrelated render too.
	const rows = useMemo(() => {
		const built = samples.map((s) => {
			// `t` is declared on the type so the row satisfies `withSessionBreaks`' constraint;
			// the stage columns are added by name below.
			const row: Record<string, number> & { t: number } = { t: tSeconds(s) };
			const byName = new Map(s.stages.map((st) => [st.name, st] as const));
			for (const n of names) {
				const st = byName.get(n);
				row[n] = st ? (p99 ? st.p99_us : st.p50_us) : 0;
			}
			return row;
		});
		return withSessionBreaks(samples, built);
	}, [samples, names, p99]);

	return (
		<div className="space-y-2">
			{toggle && (
				// The button used to be labelled with the percentile currently PLOTTED while looking
				// like an action, so it read as "click to show p99" when p99 was already showing.
				// Two explicit options, with the active one pressed, says which is which.
				<div className="flex justify-end gap-1">
					{([false, true] as const).map((wantP99) => (
						<Button
							key={String(wantP99)}
							variant={p99 === wantP99 ? "default" : "outline"}
							size="sm"
							aria-pressed={p99 === wantP99}
							onClick={() => setP99(wantP99)}
						>
							{wantP99 ? m.stats_p99() : m.stats_p50()}
						</Button>
					))}
				</div>
			)}
			<ChartFrame>
				<AreaChart
					data={rows}
					margin={{ top: 6, right: 8, left: 0, bottom: 0 }}
				>
					<CartesianGrid strokeDasharray="3 3" stroke={gridStroke} />
					<XAxis {...timeAxis} />
					<YAxis
						tick={axisTick}
						stroke={gridStroke}
						width={52}
						unit="µs"
						allowDecimals={false}
					/>
					<Tooltip contentStyle={tooltipStyle} />
					<Legend wrapperStyle={legendStyle} />
					{names.map((n, i) => (
						<Area
							key={n}
							type="monotone"
							dataKey={n}
							stackId="lat"
							stroke={colorFor(n, i)}
							fill={colorFor(n, i)}
							fillOpacity={0.5}
							isAnimationActive={false}
						/>
					))}
				</AreaChart>
			</ChartFrame>
		</div>
	);
}

/** New vs repeat fps (left axis) + tx goodput Mb/s vs the configured target (right axis). */
export function ThroughputChart({ samples }: { samples: StatsSample[] }) {
	const rows = useMemo(
		() =>
			withSessionBreaks(
				samples,
				samples.map((s) => ({
					t: tSeconds(s),
					fps: s.fps,
					repeat: s.repeat_fps,
					mbps: s.mbps,
					// The configured encoder target (kbps → Mb/s) so goodput reads against it.
					target: s.bitrate_kbps / 1000,
				})),
			),
		[samples],
	);
	return (
		<ChartFrame>
			<LineChart data={rows} margin={{ top: 6, right: 8, left: 0, bottom: 0 }}>
				<CartesianGrid strokeDasharray="3 3" stroke={gridStroke} />
				<XAxis {...timeAxis} />
				<YAxis
					yAxisId="fps"
					tick={axisTick}
					stroke={gridStroke}
					width={40}
					allowDecimals={false}
				/>
				<YAxis
					yAxisId="mbps"
					orientation="right"
					tick={axisTick}
					stroke={gridStroke}
					width={48}
				/>
				<Tooltip contentStyle={tooltipStyle} />
				<Legend wrapperStyle={legendStyle} />
				<Line
					yAxisId="fps"
					type="monotone"
					dataKey="fps"
					name={m.stats_fps_new()}
					stroke="#6c5bf3"
					dot={false}
					isAnimationActive={false}
				/>
				<Line
					yAxisId="fps"
					type="monotone"
					dataKey="repeat"
					name={m.stats_fps_repeat()}
					stroke="#f2a922"
					strokeDasharray="4 3"
					dot={false}
					isAnimationActive={false}
				/>
				<Line
					yAxisId="mbps"
					type="monotone"
					dataKey="mbps"
					name={m.stats_mbps()}
					stroke="#1fb6a8"
					dot={false}
					isAnimationActive={false}
				/>
				<Line
					yAxisId="mbps"
					type="monotone"
					dataKey="target"
					name={m.stats_bitrate_target()}
					stroke="#94a3b8"
					strokeDasharray="2 3"
					dot={false}
					isAnimationActive={false}
				/>
			</LineChart>
		</ChartFrame>
	);
}

/** Loss/recovery counters per window — frames/packets/send drops + FEC recovered. On the
 *  GameStream path only `frames` is instrumented (the receiver-side counters stay 0), so a `kind`
 *  of "gamestream" surfaces a note instead of letting three flat-zero lines read as "no loss". */
export function HealthChart({
	samples,
	kind,
}: {
	samples: StatsSample[];
	kind?: string;
}) {
	const rows = useMemo(
		() =>
			withSessionBreaks(
				samples,
				samples.map((s) => ({
					t: tSeconds(s),
					frames: s.frames_dropped,
					packets: s.packets_dropped,
					send: s.send_dropped,
					fec: s.fec_recovered,
				})),
			),
		[samples],
	);
	return (
		<>
			{kind === "gamestream" && (
				<p className="mb-2 text-xs text-muted-foreground">
					{m.stats_health_gamestream_note()}
				</p>
			)}
			<ChartFrame>
				<LineChart
					data={rows}
					margin={{ top: 6, right: 8, left: 0, bottom: 0 }}
				>
					<CartesianGrid strokeDasharray="3 3" stroke={gridStroke} />
					<XAxis {...timeAxis} />
					<YAxis
						tick={axisTick}
						stroke={gridStroke}
						width={40}
						allowDecimals={false}
					/>
					<Tooltip contentStyle={tooltipStyle} />
					<Legend wrapperStyle={legendStyle} />
					<Line
						type="monotone"
						dataKey="frames"
						name={m.stats_frames_dropped()}
						stroke="#f25c8a"
						dot={false}
						isAnimationActive={false}
					/>
					<Line
						type="monotone"
						dataKey="packets"
						name={m.stats_packets_dropped()}
						stroke="#f2a922"
						dot={false}
						isAnimationActive={false}
					/>
					<Line
						type="monotone"
						dataKey="send"
						name={m.stats_send_dropped()}
						stroke="#22a2f2"
						dot={false}
						isAnimationActive={false}
					/>
					<Line
						type="monotone"
						dataKey="fec"
						name={m.stats_fec_recovered()}
						stroke="#1fb6a8"
						dot={false}
						isAnimationActive={false}
					/>
				</LineChart>
			</ChartFrame>
		</>
	);
}
