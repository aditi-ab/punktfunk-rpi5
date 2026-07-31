import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@unom/ui/button";
import type { FC } from "react";
import {
	getListGpusQueryKey,
	useListGpus,
	useSetGpuPreference,
} from "@/api/gen/gpu/gpu";
import type { GpuState } from "@/api/gen/model";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";

/**
 * Container: the host's GPU inventory + selection. Polls (a stream starting/stopping moves the
 * "In use" badge; an eGPU can appear) and applies auto/preferred choices via the mgmt API. A
 * preference applies to the NEXT session — the help text says so.
 */
export const GpuSection: FC = () => {
	const qc = useQueryClient();
	// GPU state only moves when a session starts or ends, which the event stream reports — so this
	// is a slow safety net rather than a 5 s poll of a device enumeration.
	const gpus = useListGpus({ query: { refetchInterval: 20_000 } });
	const setPref = useSetGpuPreference();

	const apply = (mode: "auto" | "manual", gpuId?: string) =>
		setPref.mutate(
			{ data: { mode, gpu_id: gpuId ?? null } },
			{
				onSuccess: () =>
					qc.invalidateQueries({ queryKey: getListGpusQueryKey() }),
			},
		);

	return <GpuCard state={gpus} onApply={apply} busy={setPref.isPending} />;
};

const fmtVram = (mb: number) =>
	mb >= 1024 ? `${Math.round(mb / 1024)} GiB` : `${mb} MiB`;

/**
 * The vendor an explicit `PUNKTFUNK_ENCODER` pin can open on (display name) — the console mirror
 * of the host's backend→vendor table. Vendor-agnostic pins (software) and unknown/multi-vendor
 * spellings (vaapi, vulkan, pyrowave) map to nothing: no conflict to warn about.
 */
const encoderPinVendor: Record<string, string> = {
	nvenc: "NVIDIA",
	nvidia: "NVIDIA",
	cuda: "NVIDIA",
	hw: "NVIDIA",
	amf: "AMD",
	amd: "AMD",
	qsv: "Intel",
	intel: "Intel",
};

/**
 * The host.env encoder pin, surfaced so a conflicting GPU choice doesn't just look broken: amber
 * when the pin's vendor contradicts the next session's GPU (the host overrides the pin at session
 * open — the stale pin should be removed), a muted note otherwise.
 */
const EncoderPinNote: FC<{ state: GpuState; pin: string }> = ({
	state,
	pin,
}) => {
	const vendor = encoderPinVendor[pin];
	const conflicting =
		vendor && state.selected && state.selected.vendor !== vendor.toLowerCase();
	return conflicting && state.selected ? (
		<p className="text-sm text-amber-600 dark:text-amber-500">
			{m.gpu_encoder_pin_warning({
				value: pin,
				vendor,
				name: state.selected.name,
			})}
		</p>
	) : (
		<p className="text-xs text-muted-foreground">
			{m.gpu_encoder_pin_note({ value: pin })}
		</p>
	);
};

/**
 * GPU list in the compositors-card style: per-GPU badges for the manual pick ("Preferred"), what
 * the next session will use ("Next session"), and what live sessions encode on right now
 * ("In use · NVENC"), plus an Automatic/Prefer control pair.
 */
export const GpuCard: FC<{
	state: Loadable<GpuState>;
	onApply: (mode: "auto" | "manual", gpuId?: string) => void;
	busy: boolean;
}> = ({ state, onApply, busy }) => {
	const s = state.data;
	return (
		<Card>
			<CardHeader>
				<CardTitle className="flex items-center justify-between gap-4">
					<span>{m.host_gpus()}</span>
					{s && s.gpus.length > 0 && (
						<Button
							size="sm"
							variant={s.mode === "auto" ? "default" : "outline"}
							disabled={busy || s.mode === "auto"}
							onClick={() => onApply("auto")}
						>
							{m.gpu_automatic()}
						</Button>
					)}
				</CardTitle>
			</CardHeader>
			<CardContent className="space-y-4">
				<p className="text-sm text-muted-foreground">{m.host_gpus_help()}</p>
				<QueryState
					isLoading={state.isLoading}
					error={state.error}
					refetch={state.refetch}
				>
					{s &&
						(s.gpus.length === 0 ? (
							<p className="text-sm text-muted-foreground">{m.gpu_none()}</p>
						) : (
							<ul className="divide-y rounded-md border">
								{s.gpus.map((g) => {
									const isActive = s.active?.id === g.id;
									const isSelected = s.selected?.id === g.id;
									const isPreferred =
										s.mode === "manual" && s.preferred_id === g.id;
									return (
										<li
											key={g.id}
											className="flex items-center justify-between gap-4 px-4 py-3"
										>
											<div className="min-w-0">
												<div className="flex flex-wrap items-center gap-2">
													<span className="font-medium">{g.name}</span>
													{isPreferred && (
														<Badge variant="secondary">
															{m.gpu_preferred()}
														</Badge>
													)}
													{isActive && s.active ? (
														<Badge variant="success">
															{m.gpu_in_use({
																backend: s.active.backend.toUpperCase(),
															})}
														</Badge>
													) : (
														isSelected && (
															<Badge variant="default">
																{m.gpu_next_session()}
															</Badge>
														)
													)}
												</div>
												<code className="text-xs text-muted-foreground">
													{g.vendor}
													{g.vram_mb > 0 ? ` · ${fmtVram(g.vram_mb)}` : ""}
													{` · ${g.id}`}
												</code>
											</div>
											<Button
												size="sm"
												variant="outline"
												disabled={busy || isPreferred}
												onClick={() => onApply("manual", g.id)}
											>
												{m.gpu_prefer()}
											</Button>
										</li>
									);
								})}
							</ul>
						))}
					{s?.selected?.source === "preference_missing" && (
						<p className="text-sm text-amber-600 dark:text-amber-500">
							{m.gpu_missing_warning({ name: s.preferred_name ?? "?" })}
						</p>
					)}
					{s?.env_override && s.mode === "auto" && (
						<p className="text-xs text-muted-foreground">
							{m.gpu_env_note({ value: s.env_override })}
						</p>
					)}
					{s?.encoder_pin && <EncoderPinNote state={s} pin={s.encoder_pin} />}
				</QueryState>
			</CardContent>
		</Card>
	);
};
