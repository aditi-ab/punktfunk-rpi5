import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@unom/ui/button";
import { toast } from "@unom/ui/toast";
import { Pencil, Plus, RefreshCw, Trash2 } from "lucide-react";
import { type FC, type MouseEvent, type ReactNode, useEffect, useState } from "react";
import {
	getGetDisplayStateQueryKey,
	getGetDisplaySettingsQueryKey,
	useCreateCustomPreset,
	useDeleteCustomPreset,
	useGetDisplaySettings,
	useGetDisplayState,
	useReleaseDisplay,
	useSetDisplayLayout,
	useSetDisplaySettings,
	useUpdateCustomPreset,
} from "@/api/gen/display/display";
import type {
	ApiDisplayInfo,
	CustomPreset,
	DisplayPolicy,
	EffectivePolicy,
	GameSession,
	Identity,
	KeepAlive,
	LayoutMode,
	ModeConflict,
	Preset,
	Topology,
} from "@/api/gen/model";
import { ApiError } from "@/api/fetcher";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

/**
 * Container: the host's virtual-display management policy (design/display-management.md). Lets the
 * operator pick a one-click preset OR set every option by hand — all WITHOUT any client connected
 * (this is the host's *next-connect* behavior). The live-display list + multi-monitor arrangement
 * table below act on whatever is currently streaming.
 */
export const DisplaySection: FC = () => {
	const qc = useQueryClient();
	const q = useGetDisplaySettings();
	const save = useSetDisplaySettings();

	// Local edit buffer, seeded once from the server and re-seeded after every successful apply.
	const [draft, setDraft] = useState<DisplayPolicy | null>(null);
	useEffect(() => {
		if (q.data && draft === null) setDraft(q.data.settings);
	}, [q.data, draft]);

	// Apply a policy (a one-click preset, or the hand-edited Custom draft). A change takes effect on
	// the next connect; a live session keeps the display it opened on.
	const apply = (policy: DisplayPolicy) =>
		save.mutate(
			{ data: policy },
			{
				onSuccess: (res) => {
					setDraft(res.settings);
					qc.invalidateQueries({ queryKey: getGetDisplaySettingsQueryKey() });
					// The policy auto-saves on every preset pick / field edit — without a signal
					// users kept looking for a Save button. Errors stay inline (apiErrorMessage).
					toast.success(m.display_settings_saved());
				},
			},
		);

	return (
		<div className="flex flex-col gap-card">
			<Card>
				<CardHeader>
					<CardTitle>{m.display_config_title()}</CardTitle>
				</CardHeader>
				<CardContent className="space-y-4">
					<p className="max-w-prose text-sm text-muted-foreground">{m.host_displays_help()}</p>
					<QueryState isLoading={q.isLoading} error={q.error} refetch={q.refetch}>
						{q.data && draft && (
							<DisplayForm
								draft={draft}
								setDraft={setDraft}
								presets={q.data.presets}
								customPresets={q.data.custom_presets}
								apply={apply}
								busy={save.isPending}
								error={apiErrorMessage(save.error)}
							/>
						)}
					</QueryState>
				</CardContent>
			</Card>
			<Card>
				<CardHeader>
					<CardTitle>{m.display_live()}</CardTitle>
				</CardHeader>
				<CardContent>
					<LiveDisplays />
				</CardContent>
			</Card>
		</div>
	);
};

/** Preset display order — Default first (the safe baseline), the situational ones, then Custom. */
const PRESET_ORDER = [
	"default",
	"shared-desktop",
	"hotdesk",
	"workstation",
	"gaming-rig",
	"custom",
] as const;

const DisplayForm: FC<{
	draft: DisplayPolicy;
	setDraft: (p: DisplayPolicy) => void;
	presets: { id: string; summary: string; fields: EffectivePolicy }[];
	customPresets: CustomPreset[];
	apply: (p: DisplayPolicy) => void;
	busy: boolean;
	error?: string;
}> = ({ draft, setDraft, presets, customPresets, apply, busy, error }) => {
	const qc = useQueryClient();
	const createPreset = useCreateCustomPreset();
	const updatePreset = useUpdateCustomPreset();
	const deletePreset = useDeleteCustomPreset();
	const invalidateSettings = () =>
		qc.invalidateQueries({ queryKey: getGetDisplaySettingsQueryKey() });
	const presetBusy =
		createPreset.isPending || updatePreset.isPending || deletePreset.isPending;
	const presetError = apiErrorMessage(
		createPreset.error ?? updatePreset.error ?? deletePreset.error,
	);

	const preset: Preset = draft.preset ?? "custom";
	const isCustom = preset === "custom";

	// The Custom fields (defaults filled): the edit buffer when preset === "custom", and what a
	// preset→Custom switch is seeded from, so you customize starting from the current behavior.
	const customFields: EffectivePolicy = {
		keep_alive: draft.keep_alive ?? { mode: "duration", seconds: 10 },
		topology: draft.topology ?? "auto",
		mode_conflict: draft.mode_conflict ?? "separate",
		identity: draft.identity ?? "per-client",
		layout: draft.layout ?? { mode: "auto-row", positions: {} },
		max_displays: draft.max_displays ?? 4,
	};
	const effective: EffectivePolicy =
		(isCustom ? undefined : presets.find((p) => p.id === preset)?.fields) ?? customFields;

	// The five named presets apply in ONE click; "Custom" reveals the fields, seeded from the current
	// effective behavior (nothing changes until you Save).
	const pickPreset = (id: string) => {
		if (id === "custom") {
			setDraft({
				version: 1,
				preset: "custom",
				keep_alive: effective.keep_alive,
				topology: effective.topology,
				mode_conflict: effective.mode_conflict,
				identity: effective.identity,
				layout: effective.layout,
				max_displays: effective.max_displays,
				// Game-session + the experimental axes are orthogonal to the preset — carry them
				// through the Custom switch.
				game_session: draft.game_session ?? "auto",
				ddc_power_off: draft.ddc_power_off ?? false,
				pnp_disable_monitors: draft.pnp_disable_monitors ?? false,
			});
		} else {
			apply({ ...draft, preset: id as Preset });
		}
	};

	// Applying a custom preset writes a `Custom` policy carrying its saved fields + game-session (the
	// one axis a preset DOES set) — the host has no separate apply route (design/gamemode-and-…).
	const applyCustomPreset = (p: CustomPreset) =>
		apply({
			version: 1,
			preset: "custom",
			...p.fields,
			game_session: p.game_session ?? "auto",
			// The experimental axes aren't part of a preset — keep the current settings.
			ddc_power_off: draft.ddc_power_off ?? false,
			pnp_disable_monitors: draft.pnp_disable_monitors ?? false,
		});

	// A custom card is "current" when the in-force policy is a Custom one whose fields + game-session
	// value-match this preset (there is no id on DisplayPolicy — match by value).
	const customSelected = (p: CustomPreset): boolean =>
		isCustom &&
		(draft.game_session ?? "auto") === (p.game_session ?? "auto") &&
		deepEqual(effective, p.fields);
	const anyCustomSelected = customPresets.some(customSelected);

	// Save the currently-in-force behavior (built-in OR hand-edited) as a new named preset.
	const saveAsPreset = () => {
		const name = prompt(m.display_preset_name())?.trim();
		if (!name) return; // cancelled or empty
		createPreset.mutate(
			{
				data: { name, fields: effective, game_session: draft.game_session ?? "auto" },
			},
			{ onSuccess: invalidateSettings },
		);
	};
	const renamePreset = (p: CustomPreset) => {
		const name = prompt(m.display_preset_name(), p.name)?.trim();
		if (!name) return;
		updatePreset.mutate(
			{ id: p.id, data: { name, fields: p.fields, game_session: p.game_session ?? "auto" } },
			{ onSuccess: invalidateSettings },
		);
	};
	const updatePresetToCurrent = (p: CustomPreset) =>
		updatePreset.mutate(
			{
				id: p.id,
				data: { name: p.name, fields: effective, game_session: draft.game_session ?? "auto" },
			},
			{ onSuccess: invalidateSettings },
		);
	const removePreset = (p: CustomPreset) => {
		if (!confirm(m.display_preset_delete_confirm())) return;
		deletePreset.mutate({ id: p.id }, { onSuccess: invalidateSettings });
	};

	const ka = customFields.keep_alive;
	// The duration value, remembered across the Off/Keep toggle so switching back restores it.
	const [keepSecs, setKeepSecs] = useState(ka.mode === "duration" ? ka.seconds : 300);

	return (
		<div className="space-y-6">
			{/* One-click presets — a 2-up grid so each has room to breathe */}
			<div className="space-y-4">
				<Label className="mb-1 block text-base font-semibold">{m.display_preset()}</Label>
				<div className="grid gap-3 sm:grid-cols-2">
					{PRESET_ORDER.map((id) => {
						const p = presets.find((x) => x.id === id);
						const fields = id === "custom" ? undefined : p?.fields;
						const summary = id === "custom" ? m.display_custom_desc() : p?.summary;
						// The built-in "Custom" card is the hand-edit mode; when the active Custom policy
						// value-matches a saved preset, that preset's card owns the "current" ring instead.
						const selected = preset === id && !(id === "custom" && anyCustomSelected);
						const soon = DISABLED_PRESETS.has(id);
						const disabled = busy || soon;
						const pick = () => {
							if (!disabled) pickPreset(id);
						};
						return (
							<Card
								key={id}
								interactive
								role="button"
								tabIndex={disabled ? -1 : 0}
								aria-pressed={selected}
								aria-disabled={disabled || undefined}
								onClick={pick}
								onKeyDown={(e) => {
									if (e.key === "Enter" || e.key === " ") {
										e.preventDefault();
										pick();
									}
								}}
								className={cn(
									"flex h-full flex-col p-4",
									disabled ? "cursor-not-allowed opacity-60" : "cursor-pointer",
									selected && "ring-2 ring-primary",
								)}
							>
								<div className="flex items-center justify-between gap-2">
									<span className="text-base font-semibold">
										{(PRESET_LABEL[id] ?? (() => id))()}
										{soon && (
											<span className="ml-2 text-xs font-normal text-muted-foreground">
												{m.display_preset_soon()}
											</span>
										)}
									</span>
									{selected && (
										<Badge variant="success">{m.display_preset_current()}</Badge>
									)}
								</div>
								{summary && (
									<p className="mt-1 text-sm text-muted-foreground">{summary}</p>
								)}
								{fields && (
									<div className="mt-auto flex flex-wrap gap-1.5 pt-3">
										<Badge variant="secondary">{fmtKeepAlive(fields.keep_alive)}</Badge>
										<Badge variant="secondary">{tr(TOPOLOGY_LABEL, fields.topology)}</Badge>
										<Badge variant="outline">{tr(CONFLICT_LABEL, fields.mode_conflict)}</Badge>
										<Badge variant="outline">{tr(IDENTITY_LABEL, fields.identity)}</Badge>
									</div>
								)}
							</Card>
						);
					})}
				</div>
			</div>

			{/* Custom presets — the operator's saved field-bundles, rendered like the built-ins but
			    editable/deletable, plus a "Save as preset" that captures the current effective behavior. */}
			<div className="space-y-4">
				<div className="flex flex-wrap items-center justify-between gap-2">
					<Label className="text-base font-semibold">
						{m.display_preset_custom_label()}
					</Label>
					<Button
						size="sm"
						variant="outline"
						disabled={busy || presetBusy}
						onClick={saveAsPreset}
					>
						<Plus className="mr-1 size-4" />
						{m.display_preset_save_as()}
					</Button>
				</div>
				{customPresets.length > 0 && (
					<div className="grid gap-3 sm:grid-cols-2">
						{customPresets.map((p) => (
							<CustomPresetCard
								key={p.id}
								preset={p}
								selected={customSelected(p)}
								busy={busy || presetBusy}
								onApply={() => applyCustomPreset(p)}
								onRename={() => renamePreset(p)}
								onUpdate={() => updatePresetToCurrent(p)}
								onDelete={() => removePreset(p)}
							/>
						))}
					</div>
				)}
				{presetError && (
					<p className="text-sm text-amber-600 dark:text-amber-500">{presetError}</p>
				)}
			</div>

			{/* Custom: every option by hand */}
			{isCustom && (
				<div className="space-y-6 rounded-lg border p-5">
					<Field label={m.display_keep_alive()} help={m.display_keep_alive_help()}>
						<div className="flex flex-wrap items-center gap-2">
							<Button
								size="sm"
								variant={ka.mode === "off" ? "default" : "outline"}
								disabled={busy}
								onClick={() => setDraft({ ...draft, keep_alive: { mode: "off" } })}
							>
								{m.display_keep_alive_off()}
							</Button>
							<Button
								size="sm"
								variant={ka.mode === "duration" ? "default" : "outline"}
								disabled={busy}
								onClick={() =>
									setDraft({ ...draft, keep_alive: { mode: "duration", seconds: keepSecs } })
								}
							>
								{m.display_keep_alive_keep()}
							</Button>
							{ka.mode === "duration" && (
								<div className="flex items-center gap-2">
									<Input
										type="number"
										min={0}
										className="w-24"
										value={ka.seconds}
										disabled={busy}
										onChange={(e) => {
											const n = Math.max(0, Number(e.target.value) || 0);
											setKeepSecs(n);
											setDraft({ ...draft, keep_alive: { mode: "duration", seconds: n } });
										}}
									/>
									<span className="text-sm text-muted-foreground">
										{m.display_keep_alive_seconds()}
									</span>
								</div>
							)}
						</div>
					</Field>

					<Choice
						label={m.display_topology()}
						help={m.display_topology_help()}
						value={customFields.topology}
						options={["auto", "extend", "primary", "exclusive"]}
						labels={TOPOLOGY_LABEL}
						disabled={busy}
						onPick={(v) => setDraft({ ...draft, topology: v as Topology })}
					/>
					<Choice
						label={m.display_conflict()}
						help={m.display_conflict_help()}
						value={customFields.mode_conflict}
						options={["separate", "steal", "join", "reject"]}
						labels={CONFLICT_LABEL}
						disabled={busy}
						onPick={(v) => setDraft({ ...draft, mode_conflict: v as ModeConflict })}
					/>
					<Choice
						label={m.display_identity()}
						help={m.display_identity_help()}
						value={customFields.identity}
						options={["shared", "per-client", "per-client-mode"]}
						labels={IDENTITY_LABEL}
						disabled={busy}
						onPick={(v) => setDraft({ ...draft, identity: v as Identity })}
					/>
					<Choice
						label={m.display_layout_mode()}
						help={m.display_layout_help()}
						value={customFields.layout.mode ?? "auto-row"}
						options={["auto-row", "manual"]}
						labels={LAYOUT_LABEL}
						disabled={busy}
						onPick={(v) =>
							setDraft({
								...draft,
								layout: { mode: v as LayoutMode, positions: draft.layout?.positions ?? {} },
							})
						}
					/>

					<Field label={m.display_max()}>
						<Input
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
					</Field>

					<div className="border-t pt-4">
						<Button onClick={() => apply(draft)} disabled={busy}>
							{m.display_save()}
						</Button>
					</div>
				</div>
			)}

			{/* Game-session routing — orthogonal to the preset/lifecycle axes, so it lives outside the
			    Custom block and applies immediately on change (like a preset click). */}
			<div className="border-t pt-4">
				<Choice
					label={m.display_game_session()}
					help={m.display_game_session_help()}
					value={draft.game_session ?? "auto"}
					options={["auto", "dedicated"]}
					labels={GAME_SESSION_LABEL}
					disabled={busy}
					onPick={(v) => {
						const next = { ...draft, game_session: v as GameSession };
						setDraft(next);
						apply(next);
					}}
				/>
			</div>

			{/* EXPERIMENTAL toggles — orthogonal like game-session (survive preset switches, apply
			    immediately). Windows-only in effect, acted on at the Exclusive isolate. */}
			<ExperimentalToggle
				label={m.display_ddc()}
				help={m.display_ddc_help()}
				value={draft.ddc_power_off ?? false}
				offLabel={m.display_ddc_disabled()}
				onLabel={m.display_ddc_enabled()}
				busy={busy}
				onSet={(on) => {
					const next = { ...draft, ddc_power_off: on };
					setDraft(next);
					apply(next);
				}}
			/>
			<ExperimentalToggle
				label={m.display_pnp()}
				help={m.display_pnp_help()}
				value={draft.pnp_disable_monitors ?? false}
				offLabel={m.display_pnp_disabled()}
				onLabel={m.display_pnp_enabled()}
				busy={busy}
				onSet={(on) => {
					const next = { ...draft, pnp_disable_monitors: on };
					setDraft(next);
					apply(next);
				}}
			/>

			{/* What's in force right now */}
			<div className="flex flex-wrap items-center gap-2 border-t pt-3">
				<span className="text-sm text-muted-foreground">{m.display_effective()}:</span>
				<Badge variant="secondary">{fmtKeepAlive(effective.keep_alive)}</Badge>
				<Badge variant="secondary">{tr(TOPOLOGY_LABEL, effective.topology)}</Badge>
				<Badge variant="outline">{tr(CONFLICT_LABEL, effective.mode_conflict)}</Badge>
				<Badge variant="outline">{tr(IDENTITY_LABEL, effective.identity)}</Badge>
				<Badge variant="outline">{tr(LAYOUT_LABEL, effective.layout.mode)}</Badge>
				<Badge variant="outline">{`${effective.max_displays}×`}</Badge>
				{(draft.game_session ?? "auto") === "dedicated" && (
					<Badge variant="secondary">{m.display_game_session_dedicated()}</Badge>
				)}
				{(draft.ddc_power_off ?? false) && (
					<Badge variant="outline">{m.display_ddc_badge()}</Badge>
				)}
				{(draft.pnp_disable_monitors ?? false) && (
					<Badge variant="outline">{m.display_pnp_badge()}</Badge>
				)}
			</div>

			<p className="max-w-prose text-xs text-muted-foreground">{m.display_pending_note()}</p>
			{error && <p className="text-sm text-amber-600 dark:text-amber-500">{error}</p>}
		</div>
	);
};

/** A labeled config field — label, then the control, then optional help. The single source of the
 * label→control→help spacing so every field (keep-alive, the button groups, max-displays) lines up. */
const Field: FC<{ label: string; help?: string; children: ReactNode }> = ({
	label,
	help,
	children,
}) => (
	<div className="space-y-3">
		<Label className="block">{label}</Label>
		{children}
		{help && <p className="max-w-prose text-xs text-muted-foreground">{help}</p>}
	</div>
);

/**
 * An Experimental-badged on/off policy toggle (the DDC/CI and PnP monitor axes) — rendered outside
 * the Custom block like the game-session axis: survives preset switches and applies immediately.
 */
const ExperimentalToggle: FC<{
	label: string;
	help: string;
	value: boolean;
	offLabel: string;
	onLabel: string;
	busy: boolean;
	onSet: (v: boolean) => void;
}> = ({ label, help, value, offLabel, onLabel, busy, onSet }) => (
	<div className="border-t pt-4">
		<div className="space-y-3">
			<div className="flex items-center gap-2">
				<Label className="block">{label}</Label>
				<Badge variant="outline" className="text-amber-600 dark:text-amber-500">
					{m.display_experimental()}
				</Badge>
			</div>
			<div className="flex flex-wrap gap-2">
				{([false, true] as const).map((on) => (
					<Button
						key={String(on)}
						size="sm"
						variant={value === on ? "default" : "outline"}
						disabled={busy}
						onClick={() => onSet(on)}
					>
						{on ? onLabel : offLabel}
					</Button>
				))}
			</div>
			<p className="max-w-prose text-xs text-muted-foreground">{help}</p>
		</div>
	</div>
);

/** A [`Field`] whose control is a row of mutually-exclusive option buttons (topology / conflict / …). */
const Choice: FC<{
	label: string;
	help?: string;
	value: string;
	options: readonly string[];
	labels: Record<string, () => string>;
	disabled: boolean;
	onPick: (v: string) => void;
}> = ({ label, help, value, options, labels, disabled, onPick }) => (
	<Field label={label} help={help}>
		<div className="flex flex-wrap gap-2">
			{options.map((o) => (
				<Button
					key={o}
					size="sm"
					variant={value === o ? "default" : "outline"}
					disabled={disabled}
					onClick={() => onPick(o)}
				>
					{(labels[o] ?? (() => o))()}
				</Button>
			))}
		</div>
	</Field>
);

/**
 * One saved custom preset — the same interactive card as the built-ins (click to apply → writes a
 * `Custom` policy carrying `preset.fields`), plus rename / update-to-current / delete affordances
 * (each stops propagation so it doesn't also fire the card's apply). Field badges mirror the
 * built-ins; the game-session badge shows only when it isn't the default `auto`.
 */
const CustomPresetCard: FC<{
	preset: CustomPreset;
	selected: boolean;
	busy: boolean;
	onApply: () => void;
	onRename: () => void;
	onUpdate: () => void;
	onDelete: () => void;
}> = ({ preset, selected, busy, onApply, onRename, onUpdate, onDelete }) => {
	const fields = preset.fields;
	const stop = (fn: () => void) => (e: MouseEvent) => {
		e.stopPropagation();
		if (!busy) fn();
	};
	return (
		<Card
			interactive
			role="button"
			tabIndex={busy ? -1 : 0}
			aria-pressed={selected}
			aria-disabled={busy || undefined}
			onClick={() => !busy && onApply()}
			onKeyDown={(e) => {
				if (e.key === "Enter" || e.key === " ") {
					e.preventDefault();
					if (!busy) onApply();
				}
			}}
			className={cn(
				"flex h-full flex-col p-4",
				busy ? "cursor-not-allowed opacity-60" : "cursor-pointer",
				selected && "ring-2 ring-primary",
			)}
		>
			<div className="flex items-start justify-between gap-2">
				<span className="min-w-0 truncate text-base font-semibold">{preset.name}</span>
				<div className="flex shrink-0 items-center gap-1">
					{selected && <Badge variant="success">{m.display_preset_current()}</Badge>}
					<Button
						size="icon"
						variant="ghost"
						disabled={busy}
						title={m.display_preset_edit()}
						aria-label={m.display_preset_edit()}
						onClick={stop(onRename)}
					>
						<Pencil className="size-4" />
					</Button>
					<Button
						size="icon"
						variant="ghost"
						disabled={busy}
						title={m.display_preset_update()}
						aria-label={m.display_preset_update()}
						onClick={stop(onUpdate)}
					>
						<RefreshCw className="size-4" />
					</Button>
					<Button
						size="icon"
						variant="ghost"
						disabled={busy}
						title={m.display_preset_delete()}
						aria-label={m.display_preset_delete()}
						onClick={stop(onDelete)}
					>
						<Trash2 className="size-4" />
					</Button>
				</div>
			</div>
			<div className="mt-auto flex flex-wrap gap-1.5 pt-3">
				<Badge variant="secondary">{fmtKeepAlive(fields.keep_alive)}</Badge>
				<Badge variant="secondary">{tr(TOPOLOGY_LABEL, fields.topology)}</Badge>
				<Badge variant="outline">{tr(CONFLICT_LABEL, fields.mode_conflict)}</Badge>
				<Badge variant="outline">{tr(IDENTITY_LABEL, fields.identity)}</Badge>
				{(preset.game_session ?? "auto") !== "auto" && (
					<Badge variant="secondary">{tr(GAME_SESSION_LABEL, preset.game_session)}</Badge>
				)}
			</div>
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
		<div className="space-y-3">
			{kept.length > 0 && (
				<div className="flex justify-end">
					<Button
						size="sm"
						variant="outline"
						disabled={release.isPending}
						onClick={() => doRelease()}
					>
						{m.display_release_all()}
					</Button>
				</div>
			)}
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
								{d.mode} <code className="text-xs text-muted-foreground">#{slot}</code>
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

/** Presets the host can't honor yet (one-click apply would 400) are surfaced but disabled. Empty
 * now that `gaming-rig` (`keep_alive: forever`) ships: the display is Pinned (Linux + Windows) and
 * freed via Release. */
const DISABLED_PRESETS: ReadonlySet<string> = new Set<string>();

const PRESET_LABEL: Record<string, () => string> = {
	custom: m.display_preset_custom,
	default: m.display_preset_default,
	"gaming-rig": m.display_preset_gaming_rig,
	"shared-desktop": m.display_preset_shared_desktop,
	hotdesk: m.display_preset_hotdesk,
	workstation: m.display_preset_workstation,
};

const TOPOLOGY_LABEL: Record<string, () => string> = {
	auto: m.display_topology_auto,
	extend: m.display_topology_extend,
	primary: m.display_topology_primary,
	exclusive: m.display_topology_exclusive,
};

const CONFLICT_LABEL: Record<string, () => string> = {
	separate: m.display_conflict_separate,
	steal: m.display_conflict_steal,
	join: m.display_conflict_join,
	reject: m.display_conflict_reject,
};

const IDENTITY_LABEL: Record<string, () => string> = {
	shared: m.display_identity_shared,
	"per-client": m.display_identity_per_client,
	"per-client-mode": m.display_identity_per_client_mode,
};

const LAYOUT_LABEL: Record<string, () => string> = {
	"auto-row": m.display_layout_auto_row,
	manual: m.display_layout_manual,
};

const GAME_SESSION_LABEL: Record<string, () => string> = {
	auto: m.display_game_session_auto,
	dedicated: m.display_game_session_dedicated,
};

/** Structural equality for the value-match of a custom preset's fields against the effective policy
 * (handles the nested `keep_alive` variants + `layout.positions` map; key order doesn't matter). */
const deepEqual = (a: unknown, b: unknown): boolean => {
	if (a === b) return true;
	if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
	const ak = Object.keys(a as object);
	const bk = Object.keys(b as object);
	if (ak.length !== bk.length) return false;
	return ak.every((k) =>
		deepEqual((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k]),
	);
};

/** Look up a localized label, tolerating an unknown/undefined key (falls back to the raw value). */
const tr = (map: Record<string, () => string>, key: string | null | undefined): string => {
	const fn = key == null ? undefined : map[key];
	return fn ? fn() : String(key ?? "");
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
