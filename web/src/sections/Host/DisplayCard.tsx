import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@unom/ui/button";
import { type FC, useEffect, useState } from "react";
import {
	getGetDisplayStateQueryKey,
	getGetDisplaySettingsQueryKey,
	useGetDisplaySettings,
	useGetDisplayState,
	useReleaseDisplay,
	useSetDisplayLayout,
	useSetDisplaySettings,
} from "@/api/gen/display/display";
import type { ApiDisplayInfo } from "@/api/gen/model";
import { ApiError } from "@/api/fetcher";
import type {
	DisplayPolicy,
	EffectivePolicy,
	KeepAlive,
	Preset,
	Topology,
} from "@/api/gen/model";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { m } from "@/paraglide/messages";

/**
 * Container: the host's virtual-display management policy (design/display-management.md). Reads the
 * stored policy + preset expansions, lets the operator pick a preset or set Custom fields, and PUTs
 * the result — a change applies to the next session. Stage 0 enforces keep-alive + topology; the
 * other stored options are shown but marked not-yet-enforced.
 */
export const DisplaySection: FC = () => {
	const qc = useQueryClient();
	const q = useGetDisplaySettings();
	const save = useSetDisplaySettings();

	// Local edit buffer, seeded once from the server and re-seeded after a successful save.
	const [draft, setDraft] = useState<DisplayPolicy | null>(null);
	useEffect(() => {
		if (q.data && draft === null) setDraft(q.data.settings);
	}, [q.data, draft]);

	const onSave = () => {
		if (!draft) return;
		save.mutate(
			{ data: draft },
			{
				onSuccess: (res) => {
					setDraft(res.settings);
					qc.invalidateQueries({ queryKey: getGetDisplaySettingsQueryKey() });
				},
			},
		);
	};

	return (
		<Card>
			<CardHeader>
				<CardTitle>{m.host_displays()}</CardTitle>
			</CardHeader>
			<CardContent className="space-y-4">
				<p className="text-sm text-muted-foreground">{m.host_displays_help()}</p>
				<QueryState isLoading={q.isLoading} error={q.error} refetch={q.refetch}>
					{q.data && draft && (
						<DisplayForm
							draft={draft}
							setDraft={setDraft}
							presets={q.data.presets}
							onSave={onSave}
							busy={save.isPending}
							error={apiErrorMessage(save.error)}
						/>
					)}
				</QueryState>
				<LiveDisplays />
			</CardContent>
		</Card>
	);
};

/**
 * The host's live/kept virtual displays, polled from `/display/state`, each with a Release button
 * for lingering/pinned ones (active displays can't be released — that's session control).
 */
const LiveDisplays: FC = () => {
	const qc = useQueryClient();
	const state = useGetDisplayState({ query: { refetchInterval: 2_000 } });
	const release = useReleaseDisplay();
	const displays = state.data?.displays ?? [];
	const kept = displays.filter((d) => d.state !== "active");

	const doRelease = (slot?: number) =>
		release.mutate(
			{ data: { slot: slot ?? null } },
			{ onSuccess: () => qc.invalidateQueries({ queryKey: getGetDisplayStateQueryKey() }) },
		);

	return (
		<div className="space-y-2 border-t pt-4">
			<div className="flex items-center justify-between gap-4">
				<h4 className="text-sm font-medium">{m.display_live()}</h4>
				{kept.length > 0 && (
					<Button
						size="sm"
						variant="outline"
						disabled={release.isPending}
						onClick={() => doRelease()}
					>
						{m.display_release_all()}
					</Button>
				)}
			</div>
			{displays.length === 0 ? (
				<p className="text-sm text-muted-foreground">{m.display_none_live()}</p>
			) : (
				<ul className="divide-y rounded-md border">
					{displays.map((d) => (
						<DisplayRow
							key={d.slot}
							d={d}
							busy={release.isPending}
							onRelease={() => doRelease(d.slot)}
						/>
					))}
				</ul>
			)}
			<DisplayArrangement displays={displays} />
		</div>
	);
};

/**
 * The multi-monitor **arrangement** editor (design/display-management.md §6.2): an x/y table over the
 * live displays that carry a stable identity slot (the manual-layout key). Saving writes
 * `PUT /display/layout`, which switches the host to a manual layout and applies from the next connect.
 * Shown only for a ≥2-display group — arranging a single display is moot.
 */
const DisplayArrangement: FC<{ displays: ApiDisplayInfo[] }> = ({ displays }) => {
	const qc = useQueryClient();
	const saveLayout = useSetDisplayLayout();
	// Only displays with a stable identity slot can be pinned (shared/anonymous ones have no key).
	const arrangeable = displays.filter((d) => d.identity_slot != null);

	// Local edit buffer keyed by identity-slot string → {x, y}, seeded once from the current positions.
	const [pos, setPos] = useState<Record<string, { x: number; y: number }> | null>(null);
	useEffect(() => {
		if (pos === null && arrangeable.length > 0) {
			const seed: Record<string, { x: number; y: number }> = {};
			for (const d of arrangeable) seed[String(d.identity_slot)] = { x: d.x, y: d.y };
			setPos(seed);
		}
	}, [arrangeable, pos]);

	if (arrangeable.length < 2) return null;
	const cur = pos ?? {};

	const setXY = (slot: number, key: "x" | "y", val: number) => {
		const k = String(slot);
		setPos({ ...cur, [k]: { ...(cur[k] ?? { x: 0, y: 0 }), [key]: val } });
	};

	const onSave = () =>
		saveLayout.mutate(
			{ data: { positions: cur } },
			{ onSuccess: () => qc.invalidateQueries({ queryKey: getGetDisplayStateQueryKey() }) },
		);

	return (
		<div className="space-y-2 border-t pt-4">
			<h4 className="text-sm font-medium">{m.display_arrange()}</h4>
			<p className="text-xs text-muted-foreground">{m.display_arrange_help()}</p>
			<div className="space-y-2">
				{arrangeable.map((d) => {
					const slot = d.identity_slot as number;
					const p = cur[String(slot)] ?? { x: d.x, y: d.y };
					return (
						<div key={d.slot} className="flex flex-wrap items-center gap-2 text-sm">
							<span className="w-44 truncate">
								{d.mode}{" "}
								<code className="text-xs text-muted-foreground">#{slot}</code>
							</span>
							<Label className="text-xs" htmlFor={`disp-x-${slot}`}>
								X
							</Label>
							<Input
								id={`disp-x-${slot}`}
								type="number"
								className="w-24"
								value={p.x}
								disabled={saveLayout.isPending}
								onChange={(e) => setXY(slot, "x", Math.trunc(Number(e.target.value) || 0))}
							/>
							<Label className="text-xs" htmlFor={`disp-y-${slot}`}>
								Y
							</Label>
							<Input
								id={`disp-y-${slot}`}
								type="number"
								className="w-24"
								value={p.y}
								disabled={saveLayout.isPending}
								onChange={(e) => setXY(slot, "y", Math.trunc(Number(e.target.value) || 0))}
							/>
						</div>
					);
				})}
			</div>
			{saveLayout.error && (
				<p className="text-sm text-amber-600 dark:text-amber-500">
					{apiErrorMessage(saveLayout.error)}
				</p>
			)}
			<Button size="sm" onClick={onSave} disabled={saveLayout.isPending}>
				{m.display_arrange_save()}
			</Button>
		</div>
	);
};

const DisplayRow: FC<{ d: ApiDisplayInfo; busy: boolean; onRelease: () => void }> = ({
	d,
	busy,
	onRelease,
}) => {
	const active = d.state === "active";
	const stateLabel =
		d.state === "active"
			? m.display_state_active()
			: d.state === "pinned"
				? m.display_state_pinned()
				: m.display_state_lingering();
	return (
		<li className="flex items-center justify-between gap-4 px-4 py-3">
			<div className="min-w-0">
				<div className="flex flex-wrap items-center gap-2">
					<span className="font-medium">{d.mode}</span>
					<Badge variant={active ? "success" : "secondary"}>{stateLabel}</Badge>
					{active && d.sessions > 0 && (
						<Badge variant="outline">{m.display_sessions({ count: d.sessions })}</Badge>
					)}
				</div>
				<code className="text-xs text-muted-foreground">
					{d.backend}
					{d.expires_in_ms != null
						? ` · ${m.display_expires_in({ sec: Math.ceil(d.expires_in_ms / 1000) })}`
						: ""}
				</code>
			</div>
			{!active && (
				<Button size="sm" variant="outline" disabled={busy} onClick={onRelease}>
					{m.display_release_btn()}
				</Button>
			)}
		</li>
	);
};

/** The server's `{ error }` message from a thrown `ApiError` (its `.data` body), for inline display. */
const apiErrorMessage = (err: unknown): string | undefined => {
	if (err instanceof ApiError) {
		const data = err.data as { error?: string } | undefined;
		return data?.error ?? err.message;
	}
	return err ? String(err) : undefined;
};

/** The `gaming-rig` preset expands to `keep_alive: forever`, which the host rejects until the
 * display-lifecycle stage — disable it rather than let the Save 400. */
const DISABLED_PRESETS: ReadonlySet<string> = new Set(["gaming-rig"]);

const PRESET_LABEL: Record<string, () => string> = {
	custom: m.display_preset_custom,
	default: m.display_preset_default,
	"gaming-rig": m.display_preset_gaming_rig,
	"shared-desktop": m.display_preset_shared_desktop,
	hotdesk: m.display_preset_hotdesk,
	workstation: m.display_preset_workstation,
};

const TOPOLOGY_LABEL: Record<Topology, () => string> = {
	auto: m.display_topology_auto,
	extend: m.display_topology_extend,
	primary: m.display_topology_primary,
	exclusive: m.display_topology_exclusive,
};

const fmtKeepAlive = (k: KeepAlive): string => {
	switch (k.mode) {
		case "off":
			return m.display_keep_alive_off();
		case "duration":
			return `${k.seconds} ${m.display_keep_alive_seconds()}`;
		case "forever":
			return "∞";
	}
};

const DisplayForm: FC<{
	draft: DisplayPolicy;
	setDraft: (p: DisplayPolicy) => void;
	presets: { id: string; summary: string; fields: EffectivePolicy }[];
	onSave: () => void;
	busy: boolean;
	error?: string;
}> = ({ draft, setDraft, presets, onSave, busy, error }) => {
	const preset: Preset = draft.preset ?? "custom";
	const isCustom = preset === "custom";
	const keepAlive: KeepAlive = draft.keep_alive ?? { mode: "duration", seconds: 10 };
	const topology: Topology = draft.topology ?? "auto";

	// Preview the effective fields: from the selected preset's expansion, or the Custom fields.
	const effective: EffectivePolicy | undefined = isCustom
		? {
				keep_alive: keepAlive,
				topology,
				mode_conflict: draft.mode_conflict ?? "separate",
				identity: draft.identity ?? "per-client",
				layout: draft.layout ?? { mode: "auto-row", positions: {} },
				max_displays: draft.max_displays ?? 4,
			}
		: presets.find((p) => p.id === preset)?.fields;

	const presetSummary = presets.find((p) => p.id === preset)?.summary;

	const secondsValue = keepAlive.mode === "duration" ? keepAlive.seconds : 300;

	return (
		<div className="space-y-5">
			{/* Preset picker */}
			<div className="space-y-2">
				<Label>{m.display_preset()}</Label>
				<div className="flex flex-wrap gap-2">
					{(["custom", "default", "gaming-rig", "shared-desktop", "hotdesk", "workstation"] as const).map(
						(id) => (
							<Button
								key={id}
								size="sm"
								variant={preset === id ? "default" : "outline"}
								disabled={busy || DISABLED_PRESETS.has(id)}
								onClick={() => setDraft({ ...draft, preset: id as Preset })}
							>
								{(PRESET_LABEL[id] ?? (() => id))()}
							</Button>
						),
					)}
				</div>
				{presetSummary && !isCustom && (
					<p className="text-xs text-muted-foreground">{presetSummary}</p>
				)}
			</div>

			{/* Custom fields: keep-alive + topology + max displays */}
			{isCustom && (
				<div className="space-y-4 rounded-md border p-4">
					<div className="space-y-2">
						<Label>{m.display_keep_alive()}</Label>
						<div className="flex items-center gap-2">
							<Button
								size="sm"
								variant={keepAlive.mode === "off" ? "default" : "outline"}
								disabled={busy}
								onClick={() => setDraft({ ...draft, keep_alive: { mode: "off" } })}
							>
								{m.display_keep_alive_off()}
							</Button>
							<Input
								type="number"
								min={0}
								className="w-24"
								value={secondsValue}
								disabled={busy}
								onChange={(e) =>
									setDraft({
										...draft,
										keep_alive: {
											mode: "duration",
											seconds: Math.max(0, Number(e.target.value) || 0),
										},
									})
								}
							/>
							<span className="text-sm text-muted-foreground">
								{m.display_keep_alive_seconds()}
							</span>
						</div>
					</div>

					<div className="space-y-2">
						<Label>{m.display_topology()}</Label>
						<div className="flex flex-wrap gap-2">
							{(["auto", "extend", "primary", "exclusive"] as const).map((t) => (
								<Button
									key={t}
									size="sm"
									variant={topology === t ? "default" : "outline"}
									disabled={busy}
									onClick={() => setDraft({ ...draft, topology: t })}
								>
									{TOPOLOGY_LABEL[t]()}
								</Button>
							))}
						</div>
					</div>

					<div className="space-y-2">
						<Label htmlFor="disp-max">{m.display_max()}</Label>
						<Input
							id="disp-max"
							type="number"
							min={1}
							max={16}
							className="w-24"
							value={draft.max_displays ?? 4}
							disabled={busy}
							onChange={(e) =>
								setDraft({
									...draft,
									max_displays: Math.min(16, Math.max(1, Number(e.target.value) || 1)),
								})
							}
						/>
					</div>
				</div>
			)}

			{/* Effective preview */}
			{effective && (
				<div className="flex flex-wrap items-center gap-2">
					<span className="text-sm text-muted-foreground">{m.display_effective()}:</span>
					<Badge variant="secondary">{fmtKeepAlive(effective.keep_alive)}</Badge>
					<Badge variant="secondary">{TOPOLOGY_LABEL[effective.topology]()}</Badge>
					<Badge variant="outline">{effective.mode_conflict}</Badge>
					<Badge variant="outline">{effective.identity}</Badge>
					<Badge variant="outline">{`${effective.max_displays}×`}</Badge>
				</div>
			)}

			<p className="text-xs text-muted-foreground">{m.display_pending_note()}</p>

			{error && (
				<p className="text-sm text-amber-600 dark:text-amber-500">{error}</p>
			)}

			<Button onClick={onSave} disabled={busy}>
				{m.display_save()}
			</Button>
		</div>
	);
};
