import { useQueryClient } from "@tanstack/react-query";
import { useBlocker } from "@tanstack/react-router";
import { toast } from "@unom/ui/toast";
import { Pencil, Plus, RefreshCw, Trash2 } from "lucide-react";
import {
	type FC,
	type MouseEvent,
	type ReactNode,
	useEffect,
	useMemo,
	useRef,
	useState,
} from "react";
import {
	getGetDisplaySettingsQueryKey,
	getGetDisplayStateQueryKey,
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
import { QueryState } from "@/components/query-state";
import { Stagger } from "@/components/stagger";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { apiErrorMessage } from "@/lib/errors";
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

	// Local edit buffer, seeded from the server. `seeded` tracks the server value we last seeded
	// from, so we can adopt a server-side change that happens underneath us — e.g. saving the display
	// ARRANGEMENT switches `layout` to manual and locks in Custom fields (`PUT /display/layout`) —
	// WITHOUT clobbering unsaved local edits: re-seed only while the draft still matches the last
	// seed (no pending edits). Custom edits aren't auto-applied (there's an explicit Save), so a
	// naive "always re-seed on server change" would eat them.
	const [draft, setDraft] = useState<DisplayPolicy | null>(null);
	const seeded = useRef<DisplayPolicy | null>(null);
	useEffect(() => {
		if (!q.data) return;
		const server = q.data.settings;
		if (draft === null) {
			setDraft(server);
			seeded.current = server;
		} else if (
			seeded.current &&
			deepEqual(draft, seeded.current) &&
			!deepEqual(server, seeded.current)
		) {
			setDraft(server);
			seeded.current = server;
		}
	}, [q.data, draft]);

	// Apply a policy (a one-click preset, or the hand-edited Custom draft). A change takes effect on
	// the next connect; a live session keeps the display it opened on.
	const apply = (policy: DisplayPolicy) =>
		save.mutate(
			{ data: policy },
			{
				onSuccess: (res) => {
					setDraft(res.settings);
					seeded.current = res.settings;
					qc.invalidateQueries({ queryKey: getGetDisplaySettingsQueryKey() });
					// The policy auto-saves on every preset pick / field edit — without a signal
					// users kept looking for a Save button. Errors stay inline (apiErrorMessage).
					toast.success(m.display_settings_saved());
				},
			},
		);

	/**
	 * Save the hand-edited Custom block.
	 *
	 * `capture_monitor` (the streamed-screen pin) belongs to the monitor picker below, not to this
	 * form — but it is a field of the same policy object, so a draft seeded before the operator
	 * changed the streamed screen still carried the OLD value and Save quietly put it back. Defer
	 * that one axis to whatever the server currently reports.
	 */
	/** The streamed-screen pin as the HOST currently has it. Every write path defers to this rather
	 * than to the draft: the draft is only re-seeded while it is CLEAN, so once the operator has an
	 * unsaved edit its `capture_monitor` is frozen at whatever it was before they used the picker
	 * below — and any write that spreads the draft would put the old pin back. */
	const serverCaptureMonitor = () => q.data?.settings.capture_monitor ?? null;

	const saveDraft = () => {
		if (!draft) return;
		apply({ ...draft, capture_monitor: serverCaptureMonitor() });
	};

	/**
	 * Apply ONE orthogonal axis — game-session, DDC, PnP — without dragging unsaved Custom edits
	 * along for the ride.
	 *
	 * These three controls apply immediately by design, but they used to send `{...draft}`: flipping
	 * DDC while the Custom block held unsaved edits committed those edits too, and the shared
	 * `apply` then overwrote the draft with the server's answer, clearing the "unsaved" badge — so
	 * the operator got a policy they never saved with no trace it had happened. Send the axis on top
	 * of the last SAVED policy, and merge only that axis back into the draft.
	 */
	const applyAxis = (patch: Partial<DisplayPolicy>) => {
		const base = seeded.current ?? draft;
		if (!base) return;
		// Reflect the flip straight away, keeping every other unsaved edit intact.
		setDraft((d) => (d ? { ...d, ...patch } : d));
		save.mutate(
			{ data: { ...base, capture_monitor: serverCaptureMonitor(), ...patch } },
			{
				onSuccess: (res) => {
					seeded.current = res.settings;
					setDraft((d) => (d ? { ...d, ...patch } : res.settings));
					qc.invalidateQueries({ queryKey: getGetDisplaySettingsQueryKey() });
					toast.success(m.display_settings_saved());
				},
			},
		);
	};

	// Pending edits: the Custom fields do NOT auto-apply (unlike a preset click or an experimental
	// toggle), so the draft can silently diverge from what the host is actually running. Reading the
	// ref during render is safe here because every write to it is paired with a `setDraft`, so a
	// changed seed always comes with the re-render that reads it.
	const dirty =
		draft !== null &&
		seeded.current !== null &&
		!deepEqual(draft, seeded.current);
	const revert = () => {
		if (seeded.current) setDraft(seeded.current);
	};

	// Don't lose pending edits, whichever way the operator leaves.
	//
	// This used to be a bare `beforeunload` listener, which only covers a reload or a tab close —
	// clicking "Host" in the sidebar is a client-side route change the browser never hears about,
	// so the draft vanished with no prompt at all. The router's blocker covers in-app navigation
	// AND still arms `beforeunload` for the reload case, so it replaces the listener outright.
	useBlocker({
		shouldBlockFn: () => !confirm(m.display_discard_confirm()),
		enableBeforeUnload: () => dirty,
		disabled: !dirty,
	});

	return (
		<div className="flex flex-col gap-card">
			<Card>
				<CardHeader>
					<div className="flex flex-wrap items-center justify-between gap-2">
						<CardTitle>{m.display_config_title()}</CardTitle>
						{/* Visible without scrolling to the save button — the card is taller than the
						    viewport, which is exactly how the pending edits went unnoticed. */}
						{dirty && <Badge variant="warning">{m.display_unsaved()}</Badge>}
					</div>
				</CardHeader>
				<CardContent className="space-y-4">
					<p className="max-w-prose text-sm text-muted-foreground">
						{m.host_displays_help()}
					</p>
					{/* Once the form is on screen, a FAILED BACKGROUND POLL must not replace it — the
					    operator may be mid-edit, and swapping the card for an error box throws the
					    draft away to report a refetch we could simply retry. Only a failure with
					    nothing to show is worth the error state. */}
					<QueryState
						isLoading={q.isLoading}
						error={draft ? undefined : q.error}
						refetch={q.refetch}
					>
						{draft && q.error && (
							<p
								role="status"
								className="rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm"
							>
								{m.display_refresh_failed()}
							</p>
						)}
						{q.data && draft && (
							<DisplayForm
								draft={draft}
								setDraft={setDraft}
								presets={q.data.presets}
								customPresets={q.data.custom_presets}
								serverEffective={q.data.effective}
								serverCaptureMonitor={serverCaptureMonitor}
								apply={apply}
								applyAxis={applyAxis}
								saveDraft={saveDraft}
								busy={save.isPending}
								dirty={dirty}
								revert={revert}
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

/**
 * The policy form itself — pure, so Storybook can render it (with the `<Card>` wrapper the page puts
 * around it, which is also its motion parent) without a host answering `/display/settings`.
 */
export const DisplayForm: FC<{
	draft: DisplayPolicy;
	setDraft: (p: DisplayPolicy) => void;
	presets: { id: string; summary: string; fields: EffectivePolicy }[];
	customPresets: CustomPreset[];
	/** What the host reports as IN FORCE right now — not derived from the local draft. */
	serverEffective: EffectivePolicy;
	/** The streamed-screen pin as the host has it — the draft's copy goes stale while dirty. */
	serverCaptureMonitor: () => string | null;
	apply: (p: DisplayPolicy) => void;
	/** Apply one orthogonal axis on top of the SAVED policy — never the unsaved draft. */
	applyAxis: (patch: Partial<DisplayPolicy>) => void;
	/** Commit the Custom block, deferring axes this form does not own to the server's value. */
	saveDraft: () => void;
	busy: boolean;
	/** The draft differs from what the host has stored — drives the save bar + the discard guard. */
	dirty: boolean;
	/** Throw the draft away and go back to the stored policy. */
	revert: () => void;
	error?: string;
}> = ({
	draft,
	setDraft,
	presets,
	customPresets,
	serverEffective,
	serverCaptureMonitor,
	apply,
	applyAxis,
	saveDraft,
	busy,
	dirty,
	revert,
	error,
}) => {
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
		(isCustom ? undefined : presets.find((p) => p.id === preset)?.fields) ??
		customFields;

	// The five named presets apply in ONE click; "Custom" reveals the fields, seeded from the current
	// effective behavior (nothing changes until you Save).
	const pickPreset = (id: string) => {
		// A preset click overwrites the whole policy, so hand-edits that were never saved would
		// vanish without a word — the same failure as not finding the Save button, one click later.
		if (dirty && id !== "custom" && !confirm(m.display_discard_confirm()))
			return;
		// Already hand-editing: re-seeding would fill in defaults for fields the stored policy
		// leaves unset and flag "unsaved changes" for a click that changed nothing.
		if (id === "custom" && isCustom) return;
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
				// Which screen we stream is not a display-behavior axis at all — swapping the
				// streamed screen out from under the operator because they changed a preset would be
				// the worst kind of surprise. From the SERVER, not the draft (see serverCaptureMonitor).
				capture_monitor: serverCaptureMonitor(),
			});
		} else {
			apply({ ...draft, preset: id as Preset });
		}
	};

	// Applying a custom preset writes a `Custom` policy carrying its saved fields + game-session (the
	// one axis a preset DOES set) — the host has no separate apply route (design/gamemode-and-…).
	const applyCustomPreset = (p: CustomPreset) => {
		if (dirty && !confirm(m.display_discard_confirm())) return;
		apply({
			version: 1,
			preset: "custom",
			...p.fields,
			game_session: p.game_session ?? "auto",
			// The experimental axes aren't part of a preset — keep the current settings.
			ddc_power_off: draft.ddc_power_off ?? false,
			pnp_disable_monitors: draft.pnp_disable_monitors ?? false,
			// Nor is the streamed screen: this builds a FRESH policy object rather than spreading
			// the draft, so anything not named here is silently dropped — which is exactly how
			// applying a saved preset used to switch a mirroring host back to a virtual display
			// (found on-glass, .136). Every orthogonal axis has to be listed, and this one comes
			// from the SERVER (see serverCaptureMonitor).
			capture_monitor: serverCaptureMonitor(),
		});
	};

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
				data: {
					name,
					fields: effective,
					game_session: draft.game_session ?? "auto",
				},
			},
			{ onSuccess: invalidateSettings },
		);
	};
	const renamePreset = (p: CustomPreset) => {
		const name = prompt(m.display_preset_name(), p.name)?.trim();
		if (!name) return;
		updatePreset.mutate(
			{
				id: p.id,
				data: {
					name,
					fields: p.fields,
					game_session: p.game_session ?? "auto",
				},
			},
			{ onSuccess: invalidateSettings },
		);
	};
	const updatePresetToCurrent = (p: CustomPreset) =>
		updatePreset.mutate(
			{
				id: p.id,
				data: {
					name: p.name,
					fields: effective,
					game_session: draft.game_session ?? "auto",
				},
			},
			{ onSuccess: invalidateSettings },
		);
	const removePreset = (p: CustomPreset) => {
		if (!confirm(m.display_preset_delete_confirm())) return;
		deletePreset.mutate({ id: p.id }, { onSuccess: invalidateSettings });
	};

	const ka = customFields.keep_alive;
	// The duration value, remembered across the Off/Keep toggle so switching back restores it.
	const [keepSecs, setKeepSecs] = useState(
		ka.mode === "duration" ? ka.seconds : 300,
	);

	return (
		<div className="space-y-6">
			{/* One-click presets — a 2-up grid so each has room to breathe */}
			<div className="space-y-4">
				<Label className="mb-1 block text-base font-semibold">
					{m.display_preset()}
				</Label>
				{/* The preset tiles are cards nested INSIDE this page's config card, so their motion
				    parent is that card — which sets no `delayChildren` and therefore landed all six
				    on the same frame, unlike every other card grid in the console. `Stagger` gives
				    the group its own cadence back (see components/stagger.tsx). */}
				<Stagger className="grid gap-3 sm:grid-cols-2">
					{PRESET_ORDER.map((id) => {
						const p = presets.find((x) => x.id === id);
						const fields = id === "custom" ? undefined : p?.fields;
						const summary =
							id === "custom" ? m.display_custom_desc() : p?.summary;
						// The built-in "Custom" card is the hand-edit mode; when the active Custom policy
						// value-matches a saved preset, that preset's card owns the "current" ring instead.
						const selected =
							preset === id && !(id === "custom" && anyCustomSelected);
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
										<Badge variant="success">
											{m.display_preset_current()}
										</Badge>
									)}
								</div>
								{summary && (
									<p className="mt-1 text-sm text-muted-foreground">
										{summary}
									</p>
								)}
								{fields && (
									<div className="mt-auto flex flex-wrap gap-1.5 pt-3">
										<Badge variant="secondary">
											{fmtKeepAlive(fields.keep_alive)}
										</Badge>
										<Badge variant="secondary">
											{tr(TOPOLOGY_LABEL, fields.topology)}
										</Badge>
										<Badge variant="outline">
											{tr(CONFLICT_LABEL, fields.mode_conflict)}
										</Badge>
										<Badge variant="outline">
											{tr(IDENTITY_LABEL, fields.identity)}
										</Badge>
									</div>
								)}
							</Card>
						);
					})}
				</Stagger>
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
					<Stagger className="grid gap-3 sm:grid-cols-2">
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
					</Stagger>
				)}
				{presetError && (
					<p className="text-sm text-amber-600 dark:text-amber-500">
						{presetError}
					</p>
				)}
			</div>

			{/* Custom: every option by hand. Unlike everything else on this page these fields do NOT
			    auto-apply, so the block is titled, ringed while dirty, and ends in a sticky save bar. */}
			{isCustom && (
				<div
					className={cn(
						"space-y-6 rounded-lg border p-5",
						dirty && "border-[var(--warning)]",
					)}
				>
					<div className="flex flex-wrap items-center justify-between gap-2">
						<h3 className="text-base font-semibold">
							{m.display_custom_title()}
						</h3>
						{dirty && <Badge variant="warning">{m.display_unsaved()}</Badge>}
					</div>

					<Field
						label={m.display_keep_alive()}
						help={m.display_keep_alive_help()}
						group
					>
						<div className="flex flex-wrap items-center gap-2">
							<Button
								size="sm"
								variant={ka.mode === "off" ? "default" : "outline"}
								aria-pressed={ka.mode === "off"}
								disabled={busy}
								onClick={() =>
									setDraft({ ...draft, keep_alive: { mode: "off" } })
								}
							>
								{m.display_keep_alive_off()}
							</Button>
							<Button
								size="sm"
								variant={ka.mode === "duration" ? "default" : "outline"}
								aria-pressed={ka.mode === "duration"}
								disabled={busy}
								onClick={() =>
									setDraft({
										...draft,
										keep_alive: { mode: "duration", seconds: keepSecs },
									})
								}
							>
								{m.display_keep_alive_keep()}
							</Button>
							<Button
								size="sm"
								variant={ka.mode === "forever" ? "default" : "outline"}
								aria-pressed={ka.mode === "forever"}
								disabled={busy}
								onClick={() =>
									setDraft({ ...draft, keep_alive: { mode: "forever" } })
								}
							>
								{m.display_keep_alive_forever()}
							</Button>
							{ka.mode === "duration" && (
								<div className="flex items-center gap-2">
									<Input
										id="display-keep-alive-seconds"
										aria-label={m.display_keep_alive_seconds()}
										type="number"
										min={0}
										className="w-24"
										value={ka.seconds}
										disabled={busy}
										onChange={(e) => {
											const n = Math.max(0, Number(e.target.value) || 0);
											setKeepSecs(n);
											setDraft({
												...draft,
												keep_alive: { mode: "duration", seconds: n },
											});
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
						onPick={(v) =>
							setDraft({ ...draft, mode_conflict: v as ModeConflict })
						}
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
								layout: {
									mode: v as LayoutMode,
									positions: draft.layout?.positions ?? {},
								},
							})
						}
					/>

					<Field label={m.display_max()} htmlFor="display-max">
						<Input
							id="display-max"
							type="number"
							min={1}
							max={16}
							className="w-24"
							value={draft.max_displays ?? 4}
							disabled={busy}
							onChange={(e) =>
								setDraft({
									...draft,
									max_displays: Math.min(
										16,
										Math.max(1, Number(e.target.value) || 1),
									),
								})
							}
						/>
					</Field>

					{/* Sticky: the Custom block is taller than most viewports, and a save button parked
					    at its bottom edge is invisible until you scroll all the way down — people
					    edited, navigated away, and lost the lot. `bottom-0` pins it to the viewport
					    while any part of the block is on screen, and it settles into place at the end. */}
					<div
						className={cn(
							"sticky bottom-0 z-10 -mx-5 -mb-5 flex flex-wrap items-center justify-end gap-3 rounded-b-lg border-t px-5 py-3 backdrop-blur",
							dirty ? "bg-[var(--warning)]/10" : "bg-neutral/80",
						)}
					>
						<span
							className={cn(
								"mr-auto text-sm",
								dirty ? "font-medium" : "text-muted-foreground",
							)}
						>
							{dirty ? m.display_unsaved_hint() : m.display_all_saved()}
						</span>
						{dirty && (
							<Button variant="ghost" onClick={revert} disabled={busy}>
								{m.display_revert()}
							</Button>
						)}
						<Button onClick={saveDraft} disabled={busy || !dirty}>
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
					onPick={(v) => applyAxis({ game_session: v as GameSession })}
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
				onSet={(on) => applyAxis({ ddc_power_off: on })}
			/>
			<ExperimentalToggle
				label={m.display_pnp()}
				help={m.display_pnp_help()}
				value={draft.pnp_disable_monitors ?? false}
				offLabel={m.display_pnp_disabled()}
				onLabel={m.display_pnp_enabled()}
				busy={busy}
				onSet={(on) => applyAxis({ pnp_disable_monitors: on })}
			/>

			{/* What's in force right now — read from the API's `effective`, not from the local draft.
			    Deriving it from the draft meant the row restated the operator's unsaved edits back to
			    them as though the host had already adopted them. */}
			<div className="flex flex-wrap items-center gap-2 border-t pt-3">
				<span className="text-sm text-muted-foreground">
					{m.display_effective()}:
				</span>
				<Badge variant="secondary">
					{fmtKeepAlive(serverEffective.keep_alive)}
				</Badge>
				<Badge variant="secondary">
					{tr(TOPOLOGY_LABEL, serverEffective.topology)}
				</Badge>
				<Badge variant="outline">
					{tr(CONFLICT_LABEL, serverEffective.mode_conflict)}
				</Badge>
				<Badge variant="outline">
					{tr(IDENTITY_LABEL, serverEffective.identity)}
				</Badge>
				<Badge variant="outline">
					{tr(LAYOUT_LABEL, serverEffective.layout.mode)}
				</Badge>
				<Badge variant="outline">{`${serverEffective.max_displays}×`}</Badge>
				{(draft.game_session ?? "auto") === "dedicated" && (
					<Badge variant="secondary">
						{m.display_game_session_dedicated()}
					</Badge>
				)}
				{(draft.ddc_power_off ?? false) && (
					<Badge variant="outline">{m.display_ddc_badge()}</Badge>
				)}
				{(draft.pnp_disable_monitors ?? false) && (
					<Badge variant="outline">{m.display_pnp_badge()}</Badge>
				)}
			</div>

			<p className="max-w-prose text-xs text-muted-foreground">
				{m.display_pending_note()}
			</p>
			{error && (
				<p className="text-sm text-amber-600 dark:text-amber-500">{error}</p>
			)}
		</div>
	);
};

/** A labeled config field — label, then the control, then optional help. The single source of the
 * label→control→help spacing so every field (keep-alive, the button groups, max-displays) lines up. */
const Field: FC<{
	label: string;
	help?: string;
	children: ReactNode;
	/** The id of the single control this labels, when there is one — see below. */
	htmlFor?: string;
	/** Set when the field wraps a GROUP of controls rather than one input. */
	group?: boolean;
}> = ({ label, help, children, htmlFor, group }) => {
	const helpId = help && htmlFor ? `${htmlFor}-help` : undefined;
	const helpText = help && (
		<p id={helpId} className="max-w-prose text-xs text-muted-foreground">
			{help}
		</p>
	);
	// A set of related buttons IS a fieldset, so say so with the element rather than an ARIA role.
	// (The single-control case keeps a plain <label for>, which is the right pairing there.)
	if (group) {
		return (
			<fieldset className="space-y-3">
				<legend className="mb-3 block text-sm font-medium leading-none">
					{label}
				</legend>
				{children}
				{helpText}
			</fieldset>
		);
	}
	// A bare <Label> with no `htmlFor` next to an <input> with no `id` labels nothing at all: a
	// screen reader announced these as unnamed spin buttons.
	return (
		<div className="space-y-3">
			<Label className="block" htmlFor={htmlFor}>
				{label}
			</Label>
			{children}
			{helpText}
		</div>
	);
};

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
		{/* A labelled group: the pair of buttons is one control, and the label belongs to both. */}
		<fieldset className="space-y-3">
			<legend className="mb-3 flex items-center gap-2 text-sm font-medium leading-none">
				{label}
				<Badge variant="outline" className="text-amber-600 dark:text-amber-500">
					{m.display_experimental()}
				</Badge>
			</legend>
			<div className="flex flex-wrap gap-2">
				{([false, true] as const).map((on) => (
					<Button
						key={String(on)}
						size="sm"
						variant={value === on ? "default" : "outline"}
						aria-pressed={value === on}
						disabled={busy}
						onClick={() => onSet(on)}
					>
						{on ? onLabel : offLabel}
					</Button>
				))}
			</div>
			<p className="max-w-prose text-xs text-muted-foreground">{help}</p>
		</fieldset>
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
	<Field label={label} help={help} group>
		<div className="flex flex-wrap gap-2">
			{options.map((o) => (
				<Button
					key={o}
					size="sm"
					variant={value === o ? "default" : "outline"}
					// Which option is active was signalled by fill colour alone — invisible to a screen
					// reader, and to anyone who can't separate the two variants. `aria-pressed` states it.
					// (The sibling Choice in SessionGameCard already did this; these did not.)
					aria-pressed={value === o}
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
			aria-label={m.display_preset_apply_named({ name: preset.name })}
			onClick={() => !busy && onApply()}
			onKeyDown={(e) => {
				// Only when the CARD itself has focus. Keydown bubbles, so Enter/Space on the
				// rename/update/delete icons inside it also reached here and applied the preset
				// instead of running the icon's action — a keyboard user could not delete a preset.
				if (e.target !== e.currentTarget) return;
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
				<span className="min-w-0 truncate text-base font-semibold">
					{preset.name}
				</span>
				<div className="flex shrink-0 items-center gap-1">
					{selected && (
						<Badge variant="success">{m.display_preset_current()}</Badge>
					)}
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
				<Badge variant="outline">
					{tr(CONFLICT_LABEL, fields.mode_conflict)}
				</Badge>
				<Badge variant="outline">{tr(IDENTITY_LABEL, fields.identity)}</Badge>
				{(preset.game_session ?? "auto") !== "auto" && (
					<Badge variant="secondary">
						{tr(GAME_SESSION_LABEL, preset.game_session)}
					</Badge>
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
	// Create/release arrive on the event stream (api/events.ts), so the timer is only here for the
	// one thing events cannot express: the per-second "tears down in Ns" countdown on a lingering
	// display. With nothing lingering it drops to a slow safety net.
	const state = useGetDisplayState({
		query: {
			refetchInterval: (q) =>
				q.state.data?.displays?.some((d) => d.expires_in_ms != null)
					? 2_000
					: 15_000,
		},
	});
	const release = useReleaseDisplay();
	const displays = state.data?.displays ?? [];
	const kept = displays.filter((d) => d.state !== "active");

	const doRelease = (slot?: number) =>
		release.mutate(
			{ data: { slot: slot ?? null } },
			{
				onSuccess: () =>
					qc.invalidateQueries({ queryKey: getGetDisplayStateQueryKey() }),
			},
		);

	return (
		<div className="space-y-3">
			{/* Wrap in QueryState (like the settings card) so a failed/in-flight `/display/state`
			    fetch surfaces as loading/error instead of masquerading as "no live displays". */}
			<QueryState
				isLoading={state.isLoading}
				error={state.error}
				refetch={state.refetch}
			>
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
					<p className="text-sm text-muted-foreground">
						{m.display_none_live()}
					</p>
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
			</QueryState>
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
const DisplayArrangement: FC<{ displays: ApiDisplayInfo[] }> = ({
	displays,
}) => {
	const qc = useQueryClient();
	const saveLayout = useSetDisplayLayout();
	const settings = useGetDisplaySettings();
	// Every position the host has on file — including devices that are not connected right now.
	// `PUT /display/layout` REPLACES the whole map (`with_manual_layout` in pf-vdisplay builds a
	// fresh `Layout`), so anything missing from our payload is deleted. Seeding only from the live
	// displays therefore wiped the saved placement of every device that happened to be offline.
	const saved = settings.data?.settings.layout?.positions;

	// Only displays with a stable identity slot can be pinned (shared/anonymous ones have no key).
	const arrangeable = useMemo(
		() => displays.filter((d) => d.identity_slot != null),
		[displays],
	);
	// Local edit buffer keyed by identity-slot string → {x, y}. `arrangeable` is memoised, and React
	// Query's structural sharing keeps `displays` identity-stable across polls that changed nothing,
	// so this effect runs when the set of displays actually changes rather than on every poll. It is
	// idempotent regardless — it only ever fills in slots it has not seen before.
	const [pos, setPos] = useState<Record<
		string,
		{ x: number; y: number }
	> | null>(null);
	useEffect(() => {
		if (arrangeable.length === 0) return;
		setPos((prev) => {
			// Seed a display the first time we see it, and never re-seed one the operator may have
			// since edited: a display that appears mid-edit used to be left out of the buffer entirely
			// and so dropped from the save.
			const next = { ...(prev ?? {}) };
			let changed = prev === null;
			for (const d of arrangeable) {
				const k = String(d.identity_slot);
				if (!(k in next)) {
					next[k] = { x: d.x, y: d.y };
					changed = true;
				}
			}
			return changed ? next : prev;
		});
	}, [arrangeable]);

	if (arrangeable.length < 2) return null;
	const cur = pos ?? {};

	const setXY = (slot: number, key: "x" | "y", val: number) => {
		const k = String(slot);
		setPos({ ...cur, [k]: { ...(cur[k] ?? { x: 0, y: 0 }), [key]: val } });
	};

	const onSave = () =>
		saveLayout.mutate(
			// Saved-first, edits on top: the host replaces the whole map, so an absent device's
			// placement survives only if we send it back.
			{ data: { positions: { ...saved, ...cur } } },
			{
				onSuccess: () => {
					qc.invalidateQueries({ queryKey: getGetDisplayStateQueryKey() });
					// The layout save also rewrites the POLICY (switches `layout` to manual and locks
					// in the current Custom fields), so refresh the settings card too — otherwise its
					// preset ring / effective badges show pre-arrange values until a manual reload.
					qc.invalidateQueries({ queryKey: getGetDisplaySettingsQueryKey() });
				},
			},
		);

	return (
		<div className="space-y-2 border-t pt-4">
			<h4 className="text-sm font-medium">{m.display_arrange()}</h4>
			<p className="text-xs text-muted-foreground">
				{m.display_arrange_help()}
			</p>
			<div className="space-y-2">
				{arrangeable.map((d) => {
					const slot = d.identity_slot as number;
					const p = cur[String(slot)] ?? { x: d.x, y: d.y };
					return (
						<div
							key={d.slot}
							className="flex flex-wrap items-center gap-2 text-sm"
						>
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
								onChange={(e) =>
									setXY(slot, "x", Math.trunc(Number(e.target.value) || 0))
								}
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
								onChange={(e) =>
									setXY(slot, "y", Math.trunc(Number(e.target.value) || 0))
								}
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

const DisplayRow: FC<{
	d: ApiDisplayInfo;
	busy: boolean;
	onRelease: () => void;
}> = ({ d, busy, onRelease }) => {
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
						<Badge variant="outline">
							{m.display_sessions({ count: d.sessions })}
						</Badge>
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
	if (
		typeof a !== "object" ||
		typeof b !== "object" ||
		a === null ||
		b === null
	)
		return false;
	const ak = Object.keys(a as object);
	const bk = Object.keys(b as object);
	if (ak.length !== bk.length) return false;
	return ak.every((k) =>
		deepEqual(
			(a as Record<string, unknown>)[k],
			(b as Record<string, unknown>)[k],
		),
	);
};

/** Look up a localized label, tolerating an unknown/undefined key (falls back to the raw value). */
const tr = (
	map: Record<string, () => string>,
	key: string | null | undefined,
): string => {
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
