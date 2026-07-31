import { Checkbox } from "@unom/ui/form/checkbox";
import { type FC, useEffect, useState } from "react";
import type { HookEntry } from "@/api/gen/model/hookEntry";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { m } from "@/paraglide/messages";

/** The event kinds the host publishes, plus the `domain.*` wildcards the hook filter accepts.
 * Same vocabulary as the SSE `?kinds=` filter, so the two stay learnable together. */
export const EVENT_KINDS = [
	"client.*",
	"client.connected",
	"client.disconnected",
	"session.*",
	"session.started",
	"session.ended",
	"stream.*",
	"stream.started",
	"stream.stopped",
	"game.*",
	"game.running",
	"game.exited",
	"pairing.*",
	"pairing.pending",
	"pairing.completed",
	"pairing.denied",
	"display.*",
	"display.created",
	"display.released",
	"library.changed",
	"update.available",
	"update.applied",
	"host.started",
	"host.stopping",
] as const;

const EMPTY: HookEntry = { on: "session.started", run: "" };

/**
 * Add or edit one hook.
 *
 * A hook is either a shell command or a webhook — never both in this form, because "run this AND
 * post that" is two hooks and pretending otherwise makes the failure modes impossible to reason
 * about. The action kind is therefore a choice, not two optional fields.
 */
export const HookForm: FC<{
	/** The hook being edited, `EMPTY`-seeded for a new one, or null when closed. */
	value: HookEntry | null;
	onCancel: () => void;
	onSave: (hook: HookEntry) => void;
}> = ({ value, onCancel, onSave }) => {
	const [draft, setDraft] = useState<HookEntry>(EMPTY);
	const [kind, setKind] = useState<"run" | "webhook">("run");
	const [filtered, setFiltered] = useState(false);

	// Re-seed whenever a different hook is opened (the dialog stays mounted between edits).
	useEffect(() => {
		if (!value) return;
		setDraft(value);
		setKind(value.webhook ? "webhook" : "run");
		setFiltered(!!value.filter);
	}, [value]);

	const set = (patch: Partial<HookEntry>) =>
		setDraft((d) => ({ ...d, ...patch }));

	const action = kind === "run" ? (draft.run ?? "") : (draft.webhook ?? "");
	const ready = draft.on.trim().length > 0 && action.trim().length > 0;

	const commit = () => {
		// Emit exactly one action field, and drop an unticked filter entirely — leaving `{}` behind
		// would read as "filter on nothing" to anyone reading the config file later.
		const out: HookEntry = {
			on: draft.on.trim(),
			...(kind === "run"
				? { run: action.trim(), webhook: null }
				: { webhook: action.trim(), run: null }),
			...(filtered && draft.filter ? { filter: draft.filter } : {}),
			...(draft.debounce_ms ? { debounce_ms: draft.debounce_ms } : {}),
			...(draft.timeout_s ? { timeout_s: draft.timeout_s } : {}),
			...(kind === "webhook" && draft.hmac_secret_file
				? { hmac_secret_file: draft.hmac_secret_file }
				: {}),
		};
		onSave(out);
	};

	return (
		<Dialog open={value !== null} onOpenChange={(o) => !o && onCancel()}>
			<DialogContent className="max-h-[85vh] max-w-xl overflow-y-auto">
				<DialogHeader>
					<DialogTitle>{m.automation_hook_title()}</DialogTitle>
					<DialogDescription>{m.automation_hook_help()}</DialogDescription>
				</DialogHeader>

				<div className="space-y-2">
					<Label htmlFor="hook-on">{m.automation_field_on()}</Label>
					<select
						id="hook-on"
						value={draft.on}
						onChange={(e) => set({ on: e.target.value })}
						className="w-full rounded-md border bg-background px-3 py-2 text-sm"
					>
						{EVENT_KINDS.map((k) => (
							<option key={k} value={k}>
								{k}
							</option>
						))}
					</select>
					<p className="text-xs text-muted-foreground">
						{m.automation_field_on_help()}
					</p>
				</div>

				<fieldset className="space-y-2">
					<legend className="text-sm font-medium">
						{m.automation_field_action()}
					</legend>
					<div className="flex gap-2">
						{(["run", "webhook"] as const).map((k) => (
							<Button
								key={k}
								type="button"
								size="sm"
								variant={kind === k ? "default" : "outline"}
								aria-pressed={kind === k}
								onClick={() => setKind(k)}
							>
								{k === "run"
									? m.automation_action_run()
									: m.automation_action_webhook()}
							</Button>
						))}
					</div>
					<Input
						id="hook-action"
						aria-label={m.automation_field_action()}
						autoComplete="off"
						spellCheck={false}
						value={action}
						placeholder={
							kind === "run" ? "/usr/local/bin/on-stream.sh" : "https://…"
						}
						onChange={(e) =>
							set(
								kind === "run"
									? { run: e.target.value }
									: { webhook: e.target.value },
							)
						}
					/>
					<p className="text-xs text-muted-foreground">
						{kind === "run"
							? m.automation_action_run_help()
							: m.automation_action_webhook_help()}
					</p>
				</fieldset>

				{kind === "webhook" && (
					<div className="space-y-2">
						<Label htmlFor="hook-hmac">{m.automation_field_hmac()}</Label>
						<Input
							id="hook-hmac"
							autoComplete="off"
							spellCheck={false}
							value={draft.hmac_secret_file ?? ""}
							onChange={(e) => set({ hmac_secret_file: e.target.value })}
						/>
						<p className="text-xs text-muted-foreground">
							{m.automation_field_hmac_help()}
						</p>
					</div>
				)}

				<Label className="flex items-start gap-3 text-sm font-normal">
					<Checkbox
						checked={filtered}
						onCheckedChange={(n) => setFiltered(n === true)}
						className="mt-0.5"
					/>
					<span>{m.automation_field_filter()}</span>
				</Label>

				{filtered && (
					<div className="grid gap-3 sm:grid-cols-2">
						<div className="space-y-2">
							<Label htmlFor="hook-client">
								{m.automation_filter_client()}
							</Label>
							<Input
								id="hook-client"
								value={draft.filter?.client ?? ""}
								onChange={(e) =>
									set({ filter: { ...draft.filter, client: e.target.value } })
								}
							/>
						</div>
						<div className="space-y-2">
							<Label htmlFor="hook-app">{m.automation_filter_app()}</Label>
							<Input
								id="hook-app"
								value={draft.filter?.app ?? ""}
								onChange={(e) =>
									set({ filter: { ...draft.filter, app: e.target.value } })
								}
							/>
						</div>
					</div>
				)}

				<div className="grid gap-3 sm:grid-cols-2">
					<div className="space-y-2">
						<Label htmlFor="hook-debounce">
							{m.automation_field_debounce()}
						</Label>
						<Input
							id="hook-debounce"
							type="number"
							min={0}
							value={draft.debounce_ms ?? 0}
							onChange={(e) =>
								set({ debounce_ms: Number(e.target.value) || 0 })
							}
						/>
					</div>
					{kind === "run" && (
						<div className="space-y-2">
							<Label htmlFor="hook-timeout">
								{m.automation_field_timeout()}
							</Label>
							<Input
								id="hook-timeout"
								type="number"
								min={1}
								max={600}
								value={draft.timeout_s ?? 30}
								onChange={(e) =>
									set({ timeout_s: Number(e.target.value) || 30 })
								}
							/>
						</div>
					)}
				</div>

				<DialogFooter>
					<Button variant="outline" onClick={onCancel}>
						{m.common_cancel()}
					</Button>
					<Button disabled={!ready} onClick={commit}>
						{m.automation_hook_save()}
					</Button>
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
};
