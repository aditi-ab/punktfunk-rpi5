import { useQueryClient } from "@tanstack/react-query";
import { X } from "lucide-react";
import { type FC, type FormEvent, useState } from "react";
import {
	getGetLibraryQueryKey,
	useCreateCustomGame,
	useUpdateCustomGame,
} from "@/api/gen/library/library";
import type { CustomInput } from "@/api/gen/model/customInput";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { m } from "@/paraglide/messages";
import { customId } from "./helpers";

interface FormState {
	title: string;
	portrait: string;
	hero: string;
	header: string;
	command: string;
}

const emptyForm: FormState = {
	title: "",
	portrait: "",
	hero: "",
	header: "",
	command: "",
};

function formFrom(entry: GameEntry): FormState {
	return {
		title: entry.title,
		portrait: entry.art.portrait ?? "",
		hero: entry.art.hero ?? "",
		header: entry.art.header ?? "",
		command: entry.launch?.kind === "command" ? entry.launch.value : "",
	};
}

/** Map the form to the API body — only attach `launch` when a command was given. */
function toInput(f: FormState): CustomInput {
	const trim = (s: string) => {
		const t = s.trim();
		return t ? t : undefined;
	};
	const command = f.command.trim();
	return {
		title: f.title.trim(),
		art: {
			portrait: trim(f.portrait),
			hero: trim(f.hero),
			header: trim(f.header),
		},
		launch: command ? { kind: "command", value: command } : null,
	};
}

/** What the form targets: an existing custom entry to edit, or "new" for a fresh add. */
export type FormTarget = GameEntry | "new";

/**
 * Container: the add/edit form — owns the create + update mutations and derives the
 * initial field state from the target. Kept entirely separate from the overview grid
 * (own file, own queries) so the two concerns don't share a component.
 */
export const GameFormSection: FC<{
	target: FormTarget;
	onClose: () => void;
}> = ({ target, onClose }) => {
	const qc = useQueryClient();
	const create = useCreateCustomGame();
	const update = useUpdateCustomGame();
	const invalidate = () =>
		qc.invalidateQueries({ queryKey: getGetLibraryQueryKey() });

	const onSubmit = async (data: CustomInput) => {
		if (target === "new") await create.mutateAsync({ data }).then(invalidate);
		else
			await update.mutateAsync({ id: customId(target), data }).then(invalidate);
		onClose();
	};

	return (
		<GameForm
			initial={target === "new" ? emptyForm : formFrom(target)}
			mode={target === "new" ? "add" : "edit"}
			onSubmit={onSubmit}
			onCancel={onClose}
			isSaving={create.isPending || update.isPending}
		/>
	);
};

/**
 * The add/edit form card. Owns only its own field state (re-seeded per mount — the
 * parent keys it by target); reports a ready-to-send `CustomInput` on submit.
 */
export const GameForm: FC<{
	initial: FormState;
	mode: "add" | "edit";
	onSubmit: (data: CustomInput) => void;
	onCancel: () => void;
	isSaving: boolean;
}> = ({ initial, mode, onSubmit, onCancel, isSaving }) => {
	const [form, setForm] = useState<FormState>(initial);

	const handleSubmit = (e: FormEvent) => {
		e.preventDefault();
		const data = toInput(form);
		if (!data.title) return;
		onSubmit(data);
	};

	return (
		<Card className="max-w-xl">
			<CardHeader className="flex-row items-center justify-between space-y-0">
				<CardTitle>
					{mode === "edit" ? m.library_edit_title() : m.library_add_title()}
				</CardTitle>
				<Button
					variant="ghost"
					size="icon"
					aria-label={m.library_cancel()}
					onClick={onCancel}
				>
					<X className="size-4" />
				</Button>
			</CardHeader>
			<CardContent>
				<form onSubmit={handleSubmit} className="space-y-4">
					<div className="space-y-2">
						<Label htmlFor="lib-title">{m.library_field_title()}</Label>
						<Input
							id="lib-title"
							required
							value={form.title}
							onChange={(e) =>
								setForm((f) => ({ ...f, title: e.target.value }))
							}
						/>
					</div>
					<div className="space-y-2">
						<Label htmlFor="lib-portrait">{m.library_field_portrait()}</Label>
						<Input
							id="lib-portrait"
							type="url"
							inputMode="url"
							value={form.portrait}
							onChange={(e) =>
								setForm((f) => ({ ...f, portrait: e.target.value }))
							}
						/>
					</div>
					<div className="space-y-2">
						<Label htmlFor="lib-hero">{m.library_field_hero()}</Label>
						<Input
							id="lib-hero"
							type="url"
							inputMode="url"
							value={form.hero}
							onChange={(e) => setForm((f) => ({ ...f, hero: e.target.value }))}
						/>
					</div>
					<div className="space-y-2">
						<Label htmlFor="lib-header">{m.library_field_header()}</Label>
						<Input
							id="lib-header"
							type="url"
							inputMode="url"
							value={form.header}
							onChange={(e) =>
								setForm((f) => ({ ...f, header: e.target.value }))
							}
						/>
					</div>
					<div className="space-y-2">
						<Label htmlFor="lib-command">{m.library_field_command()}</Label>
						<Input
							id="lib-command"
							value={form.command}
							onChange={(e) =>
								setForm((f) => ({ ...f, command: e.target.value }))
							}
						/>
						<p className="text-xs text-muted-foreground">
							{m.library_field_command_help()}
						</p>
					</div>
					<div className="flex gap-2">
						<Button type="submit" disabled={isSaving || !form.title.trim()}>
							{mode === "edit" ? m.library_save() : m.library_create()}
						</Button>
						<Button type="button" variant="outline" onClick={onCancel}>
							{m.library_cancel()}
						</Button>
					</div>
				</form>
			</CardContent>
		</Card>
	);
};
