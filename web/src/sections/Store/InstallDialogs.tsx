import { Checkbox } from "@unom/ui/form/checkbox";
import { BadgeCheck, ShieldAlert, ShieldQuestion } from "lucide-react";
import { type FC, useState } from "react";
import type { StoreEntry } from "@/api/store";
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

// Friction proportional to trust. The three install paths deliberately do NOT share a confirm
// button: a verified install is one click on a plain dialog, an external one makes you read that
// unom didn't review the code and names who did curate it, and a raw package spec makes you retype
// the spec and tick a box. The last one is never reachable by accident — no card, no button on a
// listing, only the small footer link on Browse.

/**
 * Tiers 1 and 2: install a curated catalog entry. Verified entries get an ordinary confirm; an
 * entry from an operator-added source gets the warning treatment and names its source, because
 * "pinned and integrity-checked" is not the same claim as "reviewed".
 */
export const InstallDialog: FC<{
	/** The entry being confirmed, or null when the dialog is closed. */
	entry: StoreEntry | null;
	onCancel: () => void;
	onConfirm: (entry: StoreEntry) => void;
	isPending: boolean;
}> = ({ entry, onCancel, onConfirm, isPending }) => {
	const external = entry?.tier === "external";
	return (
		<Dialog open={entry !== null} onOpenChange={(open) => !open && onCancel()}>
			{entry && (
				<DialogContent>
					<DialogHeader>
						<DialogTitle className="flex items-center gap-2">
							{external ? (
								<ShieldQuestion className="size-5 shrink-0 text-amber-600 dark:text-amber-500" />
							) : (
								<BadgeCheck className="size-5 shrink-0 text-[var(--success)]" />
							)}
							{external
								? m.store_install_external_title({ title: entry.title })
								: m.store_install_title({ title: entry.title })}
						</DialogTitle>
						<DialogDescription>
							{external
								? m.store_install_external_body({
										version: entry.version,
										source: entry.source,
									})
								: m.store_install_verified_body({ version: entry.version })}
						</DialogDescription>
					</DialogHeader>

					<p className="rounded-md bg-muted px-3 py-2 font-mono text-xs text-muted-foreground">
						{entry.pkg}@{entry.version}
					</p>

					{external && (
						<p className="rounded-md border border-amber-600/40 bg-amber-500/10 px-3 py-2 text-sm text-amber-600 dark:border-amber-500/40 dark:text-amber-500">
							{m.store_install_external_note()}
						</p>
					)}

					<DialogFooter>
						<Button variant="outline" onClick={onCancel} disabled={isPending}>
							{m.common_cancel()}
						</Button>
						{/* Red is reserved for the raw-spec install; an external one escalates through
						    its copy and its amber panel, not by borrowing the danger colour. */}
						<Button disabled={isPending} onClick={() => onConfirm(entry)}>
							{external
								? m.store_install_external_confirm()
								: m.store_install_confirm()}
						</Button>
					</DialogFooter>
				</DialogContent>
			)}
		</Dialog>
	);
};

/**
 * Tier 3: install a raw package spec. No catalog, no review, no pinning — so the dialog spells out
 * exactly what that means and asks for two independent confirmations (retype the spec, tick the
 * box) before it will even enable its button.
 */
export const SpecInstallDialog: FC<{
	open: boolean;
	onCancel: () => void;
	onConfirm: (spec: string) => void;
	isPending: boolean;
}> = ({ open, onCancel, onConfirm, isPending }) => {
	const [spec, setSpec] = useState("");
	const [echo, setEcho] = useState("");
	const [accepted, setAccepted] = useState(false);

	// Both confirmations are cleared on every exit, cancel AND confirm alike: reopening this dialog
	// must never find it pre-armed with the last spec and a ticked box.
	const reset = () => {
		setSpec("");
		setEcho("");
		setAccepted(false);
	};
	const close = () => {
		reset();
		onCancel();
	};

	const wanted = spec.trim();
	// Both gates must pass: the retyped spec matches exactly, AND the box is ticked.
	const ready = wanted.length > 0 && echo.trim() === wanted && accepted;

	return (
		<Dialog open={open} onOpenChange={(next) => !next && close()}>
			<DialogContent className="max-w-xl">
				<DialogHeader>
					<DialogTitle className="flex items-center gap-2">
						<ShieldAlert className="size-5 shrink-0 text-destructive" />
						{m.store_spec_title()}
					</DialogTitle>
					<DialogDescription>{m.store_spec_lead()}</DialogDescription>
				</DialogHeader>

				<p className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
					{m.store_spec_permanent()}
				</p>

				<div className="space-y-2">
					<Label htmlFor="store-spec">{m.store_spec_field()}</Label>
					<Input
						id="store-spec"
						autoComplete="off"
						spellCheck={false}
						placeholder="@scope/plugin-name@1.2.3"
						value={spec}
						onChange={(e) => setSpec(e.target.value)}
					/>
					<p className="text-xs text-muted-foreground">
						{m.store_spec_field_help()}
					</p>
				</div>

				<div className="space-y-2">
					<Label htmlFor="store-spec-echo">
						{m.store_spec_confirm_field()}
					</Label>
					<Input
						id="store-spec-echo"
						autoComplete="off"
						spellCheck={false}
						value={echo}
						onChange={(e) => setEcho(e.target.value)}
					/>
				</div>

				<Label className="flex items-start gap-3 text-sm font-normal">
					<Checkbox
						checked={accepted}
						onCheckedChange={(next) => setAccepted(next === true)}
						className="mt-0.5"
					/>
					<span>{m.store_spec_checkbox()}</span>
				</Label>

				<DialogFooter>
					<Button variant="outline" onClick={close} disabled={isPending}>
						{m.common_cancel()}
					</Button>
					<Button
						variant="destructive"
						disabled={!ready || isPending}
						onClick={() => {
							reset();
							onConfirm(wanted);
						}}
					>
						{m.store_spec_confirm()}
					</Button>
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
};
