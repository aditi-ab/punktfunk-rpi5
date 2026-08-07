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
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";
import { customId } from "./helpers";

interface FormState {
	title: string;
	portrait: string;
	hero: string;
	header: string;
	logo: string;
	command: string;
	/** Console-password re-confirmation, required only when `command` is set — see the field's
	 *  own comment at the render site (2026-08-05 review M-6). Never round-tripped from the
	 *  server, so it is always empty on open, including when editing an entry that has one. */
	password: string;
	/** `true` = this entry opens a launcher rather than a game (design D4). Purely presentational:
	 *  the console groups launcher entries into their own rail, and clients that don't know the
	 *  field render them as ordinary tiles. */
	isLauncher: boolean;
	// Details — the flattened GameMeta fields; numbers and lists are kept as the raw
	// text the user typed and only parsed on submit.
	platform: string;
	description: string;
	developer: string;
	publisher: string;
	releaseYear: string;
	genres: string;
	tags: string;
	region: string;
	players: string;
}

const emptyForm: FormState = {
	title: "",
	portrait: "",
	hero: "",
	header: "",
	logo: "",
	command: "",
	password: "",
	isLauncher: false,
	platform: "",
	description: "",
	developer: "",
	publisher: "",
	releaseYear: "",
	genres: "",
	tags: "",
	region: "",
	players: "",
};

function formFrom(entry: GameEntry): FormState {
	return {
		title: entry.title,
		portrait: entry.art.portrait ?? "",
		hero: entry.art.hero ?? "",
		header: entry.art.header ?? "",
		logo: entry.art.logo ?? "",
		command: entry.launch?.kind === "command" ? entry.launch.value : "",
		password: "",
		// Round-tripped like every other field: `update_custom` REPLACES the whole entry, so an
		// unread field here would silently demote a launcher entry back to a game on any edit.
		isLauncher: entry.role === "launcher",
		platform: entry.platform ?? "",
		description: entry.description ?? "",
		developer: entry.developer ?? "",
		publisher: entry.publisher ?? "",
		releaseYear: entry.release_year?.toString() ?? "",
		genres: entry.genres?.join(", ") ?? "",
		tags: entry.tags?.join(", ") ?? "",
		region: entry.region ?? "",
		players: entry.players?.toString() ?? "",
	};
}

/** Map the form to the API body — only attach `launch` when a command was given. `update_custom`
 * REPLACES the whole entry (art AND the metadata fields), so every field the form knows must
 * round-trip (else editing a game with a `logo` or a `platform` would silently drop it). */
function toInput(f: FormState): CustomInput {
	const trim = (s: string) => {
		const t = s.trim();
		return t ? t : undefined;
	};
	// "RPG, Platformer" → ["RPG", "Platformer"]; empty input → omitted entirely.
	const list = (s: string) => {
		const items = s
			.split(",")
			.map((x) => x.trim())
			.filter(Boolean);
		return items.length ? items : undefined;
	};
	const int = (s: string) => {
		const n = Number.parseInt(s.trim(), 10);
		return Number.isFinite(n) ? n : undefined;
	};
	const command = f.command.trim();
	return {
		title: f.title.trim(),
		art: {
			portrait: trim(f.portrait),
			hero: trim(f.hero),
			header: trim(f.header),
			logo: trim(f.logo),
		},
		launch: command ? { kind: "command", value: command } : null,
		// The BFF re-verifies this and strips it before forwarding; the host never sees the field.
		// Only sent when there is a command to authorize, matching the conditional gate.
		...(command ? { password: f.password } : {}),
		// Omitted when it is the default, matching the host's skip-when-`game` serialization.
		...(f.isLauncher ? { role: "launcher" as const } : {}),
		platform: trim(f.platform),
		description: trim(f.description),
		developer: trim(f.developer),
		publisher: trim(f.publisher),
		release_year: int(f.releaseYear),
		genres: list(f.genres),
		tags: list(f.tags),
		region: trim(f.region),
		players: int(f.players),
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

	// A rejected save must not close the form and must not look like a success. It used to do both:
	// nothing read `create.error`/`update.error`, and the un-caught `mutateAsync` rejection meant
	// the entry silently didn't save while the dialog disappeared — taking the operator's typing
	// with it.
	const onSubmit = async (data: CustomInput) => {
		try {
			if (target === "new") await create.mutateAsync({ data });
			else await update.mutateAsync({ id: customId(target), data });
		} catch {
			return; // the message is rendered from the mutation's own error state below
		}
		invalidate();
		onClose();
	};

	return (
		<GameForm
			initial={target === "new" ? emptyForm : formFrom(target)}
			mode={target === "new" ? "add" : "edit"}
			onSubmit={onSubmit}
			onCancel={onClose}
			isSaving={create.isPending || update.isPending}
			error={apiErrorMessage(create.error ?? update.error)}
		/>
	);
};

/** One labeled text input bound to a FormState key — the form is a stack of these. */
const Field: FC<{
	id: keyof FormState;
	label: string;
	value: string;
	onChange: (value: string) => void;
	help?: string;
	type?: string;
	required?: boolean;
}> = ({ id, label, value, onChange, help, type, required }) => (
	<div className="space-y-2">
		<Label htmlFor={`lib-${id}`}>{label}</Label>
		<Input
			id={`lib-${id}`}
			type={type}
			inputMode={
				type === "url" ? "url" : type === "number" ? "numeric" : undefined
			}
			required={required}
			value={value}
			onChange={(e) => onChange(e.target.value)}
		/>
		{help && <p className="text-xs text-muted-foreground">{help}</p>}
	</div>
);

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
	/** The host's refusal, if the last save failed — shown next to the button that caused it. */
	error?: string;
}> = ({ initial, mode, onSubmit, onCancel, isSaving, error }) => {
	const [form, setForm] = useState<FormState>(initial);
	const set = (key: keyof FormState) => (value: string) =>
		setForm((f) => ({ ...f, [key]: value }));

	const handleSubmit = (e: FormEvent) => {
		e.preventDefault();
		const data = toInput(form);
		if (!data.title) return;
		// A command is code the host will run on its own; the password field is required with it.
		if (form.command.trim() && !form.password) return;
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
					<Field
						id="title"
						label={m.library_field_title()}
						value={form.title}
						onChange={set("title")}
						required
					/>
					<Field
						id="portrait"
						label={m.library_field_portrait()}
						value={form.portrait}
						onChange={set("portrait")}
						type="url"
					/>
					<Field
						id="hero"
						label={m.library_field_hero()}
						value={form.hero}
						onChange={set("hero")}
						type="url"
					/>
					<Field
						id="header"
						label={m.library_field_header()}
						value={form.header}
						onChange={set("header")}
						type="url"
					/>
					<Field
						id="logo"
						label={m.library_field_logo()}
						value={form.logo}
						onChange={set("logo")}
						type="url"
					/>
					<Field
						id="command"
						label={m.library_field_command()}
						value={form.command}
						onChange={set("command")}
						help={m.library_field_command_help()}
					/>
					{/* A launch command is a shell command the host runs as the host user, so saving
					    one clears the same bar as a hook or an unreviewed install: the console
					    password, not just a 7-day session cookie (2026-08-05 review M-6). Shown only
					    when there is a command to authorize — gating an ordinary title/art edit
					    would just train the operator to type it without reading. */}
					{form.command.trim() && (
						<Field
							id="password"
							label={m.library_field_password()}
							value={form.password}
							onChange={set("password")}
							help={m.library_field_password_help()}
							type="password"
							required
						/>
					)}
					{/* Design D4: a launcher entry opens the launcher itself rather than a title. It
					    launches and leases like any other entry — this only moves it into the
					    console's Launchers rail. Hand-adding one is the supported way to get a
					    "Heroic" or "Lutris" tile without installing that source's plugin. */}
					<div className="space-y-2">
						<div className="flex items-center gap-2">
							<Checkbox
								id="lib-isLauncher"
								checked={form.isLauncher}
								onCheckedChange={(next) =>
									setForm((f) => ({ ...f, isLauncher: next === true }))
								}
							/>
							<Label htmlFor="lib-isLauncher">{m.library_field_role()}</Label>
						</div>
						<p className="text-xs text-muted-foreground">
							{m.library_field_role_help()}
						</p>
					</div>
					<fieldset className="space-y-4 border-t pt-2">
						<legend className="sr-only">{m.library_details_legend()}</legend>
						<p
							aria-hidden
							className="text-sm font-medium text-muted-foreground"
						>
							{m.library_details_legend()}
						</p>
						<Field
							id="platform"
							label={m.library_field_platform()}
							value={form.platform}
							onChange={set("platform")}
							help={m.library_field_platform_help()}
						/>
						<Field
							id="description"
							label={m.library_field_description()}
							value={form.description}
							onChange={set("description")}
						/>
						<div className="grid grid-cols-2 gap-4">
							<Field
								id="developer"
								label={m.library_field_developer()}
								value={form.developer}
								onChange={set("developer")}
							/>
							<Field
								id="publisher"
								label={m.library_field_publisher()}
								value={form.publisher}
								onChange={set("publisher")}
							/>
						</div>
						{/* These two stay `type="number"` over a STRING field rather than becoming
						    `InputNumber` like the policy card's numbers: both are OPTIONAL metadata
						    where empty means "don't send it" (see `int()` above), and InputNumber's
						    contract is `value: number` — it cannot express "unset", so adopting it
						    would invent a year for every entry that hasn't got one. The type here
						    only asks for a numeric keypad. */}
						<div className="grid grid-cols-2 gap-4">
							<Field
								id="releaseYear"
								label={m.library_field_release_year()}
								value={form.releaseYear}
								onChange={set("releaseYear")}
								type="number"
							/>
							<Field
								id="players"
								label={m.library_field_players()}
								value={form.players}
								onChange={set("players")}
								type="number"
							/>
						</div>
						<Field
							id="region"
							label={m.library_field_region()}
							value={form.region}
							onChange={set("region")}
							help={m.library_field_region_help()}
						/>
						<Field
							id="genres"
							label={m.library_field_genres()}
							value={form.genres}
							onChange={set("genres")}
							help={m.library_field_genres_help()}
						/>
						<Field
							id="tags"
							label={m.library_field_tags()}
							value={form.tags}
							onChange={set("tags")}
							help={m.library_field_tags_help()}
						/>
					</fieldset>
					{/* Data-loss warning, not a nicety.
					`PUT /library/custom/{id}` REPLACES the entry (host: library/custom.rs
					`update_custom` assigns `slot.prep = input.prep; slot.detect = input.detect`),
					but `GET /library` returns a `GameEntry`, which carries neither field. So the
					console cannot round-trip them — anything configured outside this form is dropped
					by a save it did not intend to touch. The real fix is host-side (expose `detect`
					and `prep` on the read model); until then, say so before the operator finds out. */}
					{mode === "edit" && (
						<p className="rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm">
							{m.library_edit_overwrites()}
						</p>
					)}
					{error && (
						<p
							role="alert"
							className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive"
						>
							{error}
						</p>
					)}
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
