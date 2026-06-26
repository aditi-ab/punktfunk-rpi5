import { Pencil, Plus, Trash2, X } from "lucide-react";
import { type FC, useState } from "react";
import type { CustomInput } from "@/api/gen/model/customInput";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import { QueryState } from "@/components/query-state";
import { Section } from "@/components/section";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";

/** The custom-CRUD path param is the raw id without the `custom:` prefix. */
function customId(entry: GameEntry): string {
	return entry.id.startsWith("custom:")
		? entry.id.slice("custom:".length)
		: entry.id;
}

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

export const LibraryView: FC<{
	library: Loadable<GameEntry[]>;
	onCreate: (data: CustomInput) => Promise<unknown>;
	onUpdate: (id: string, data: CustomInput) => Promise<unknown>;
	onDelete: (id: string) => Promise<unknown>;
	isSaving: boolean;
	isDeleting: boolean;
}> = ({ library, onCreate, onUpdate, onDelete, isSaving, isDeleting }) => {
	// null = form hidden; "" = adding a new entry; an id = editing that custom entry.
	const [editing, setEditing] = useState<string | null>(null);
	const [form, setForm] = useState<FormState>(emptyForm);

	const games = library.data ?? [];

	const openAdd = () => {
		setForm(emptyForm);
		setEditing("");
	};
	const openEdit = (entry: GameEntry) => {
		setForm(formFrom(entry));
		setEditing(customId(entry));
	};
	const closeForm = () => setEditing(null);

	const handleSubmit = async (e: React.FormEvent) => {
		e.preventDefault();
		const data = toInput(form);
		if (!data.title) return;
		await (editing ? onUpdate(editing, data) : onCreate(data));
		closeForm();
	};

	const handleDelete = async (entry: GameEntry) => {
		if (!confirm(m.library_delete_confirm())) return;
		await onDelete(customId(entry));
	};

	return (
		<Section>
			<div className="flex items-center justify-between gap-4">
				<h1 className="text-2xl font-semibold">{m.library_title()}</h1>
				{editing === null && (
					<Button onClick={openAdd}>
						<Plus className="size-4" />
						{m.library_add_button()}
					</Button>
				)}
			</div>

			{editing !== null && (
				<Card className="max-w-xl">
					<CardHeader className="flex-row items-center justify-between space-y-0">
						<CardTitle>
							{editing ? m.library_edit_title() : m.library_add_title()}
						</CardTitle>
						<Button
							variant="ghost"
							size="icon"
							aria-label={m.library_cancel()}
							onClick={closeForm}
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
								<Label htmlFor="lib-portrait">
									{m.library_field_portrait()}
								</Label>
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
									onChange={(e) =>
										setForm((f) => ({ ...f, hero: e.target.value }))
									}
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
									{editing ? m.library_save() : m.library_create()}
								</Button>
								<Button type="button" variant="outline" onClick={closeForm}>
									{m.library_cancel()}
								</Button>
							</div>
						</form>
					</CardContent>
				</Card>
			)}

			<QueryState
				isLoading={library.isLoading}
				error={library.error}
				refetch={library.refetch}
			>
				{games.length === 0 ? (
					<Card>
						<CardContent className="p-8 text-center text-sm text-muted-foreground">
							{m.library_empty()}
						</CardContent>
					</Card>
				) : (
					<div className="grid grid-cols-2 gap-4 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-5">
						{games.map((game) => (
							<GameCard
								key={game.id}
								game={game}
								onEdit={() => openEdit(game)}
								onDelete={() => handleDelete(game)}
								deleting={isDeleting}
							/>
						))}
					</div>
				)}
			</QueryState>
		</Section>
	);
};

interface GameCardProps {
	game: GameEntry;
	onEdit: () => void;
	onDelete: () => void;
	deleting: boolean;
}

/**
 * A poster tile. The cover prefers the 2:3 portrait capsule; on a load error it
 * falls back to the wide header, then to a text placeholder. Custom entries get
 * edit/delete affordances.
 */
const GameCard: FC<GameCardProps> = ({ game, onEdit, onDelete, deleting }) => {
	const isCustom = game.store === "custom";
	// Track which sources have failed so the <img> can step down portrait → header → placeholder.
	const [failed, setFailed] = useState<Record<string, boolean>>({});

	const candidates = [game.art.portrait, game.art.header].filter(
		(u): u is string => !!u && !failed[u],
	);
	const src = candidates[0];

	return (
		<Card className="group relative overflow-hidden">
			<div className="relative aspect-[2/3] bg-muted">
				{src ? (
					<img
						src={src}
						alt={game.title}
						loading="lazy"
						className="size-full object-cover"
						onError={() => setFailed((prev) => ({ ...prev, [src]: true }))}
					/>
				) : (
					<div className="flex size-full items-center justify-center p-3 text-center text-sm font-medium text-muted-foreground">
						{game.title}
					</div>
				)}
				<div className="absolute left-2 top-2">
					<Badge
						variant={isCustom ? "secondary" : "outline"}
						className="bg-background/80 backdrop-blur"
					>
						{isCustom ? m.library_store_custom() : m.library_store_steam()}
					</Badge>
				</div>
				{isCustom && (
					<div className="absolute right-2 top-2 flex gap-1 opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100">
						<Button
							variant="secondary"
							size="icon"
							className="size-7 bg-background/80 backdrop-blur"
							aria-label={m.library_edit()}
							onClick={onEdit}
						>
							<Pencil className="size-3.5" />
						</Button>
						<Button
							variant="secondary"
							size="icon"
							className="size-7 bg-background/80 backdrop-blur"
							aria-label={m.library_delete()}
							disabled={deleting}
							onClick={onDelete}
						>
							<Trash2 className="size-3.5 text-destructive" />
						</Button>
					</div>
				)}
			</div>
			<div className="truncate p-2 text-sm font-medium" title={game.title}>
				{game.title}
			</div>
		</Card>
	);
};
