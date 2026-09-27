import { Link } from "@tanstack/react-router";
import { ArrowLeft, Eye, EyeOff, Trash2 } from "lucide-react";
import { type FC, useState } from "react";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { LauncherIcon } from "@/components/launcher-icon";
import {
	PasswordConfirmField,
	type PasswordFailure,
} from "@/components/password-confirm";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages";

export interface EntryHeaderProps {
	/** The catalog entry; null while creating. */
	entry: OperatorGameEntry | null;
	/** The draft's title, so the heading follows the typing. */
	title: string;
	/** Store badge text. */
	storeName: string | null;
	/** The source that owns a read-only entry. */
	managedBy: string | null;
	dirty: boolean;
	saving: boolean;
	/** The save carries a command: the console password field shows. */
	gated: boolean;
	password: string;
	onPassword: (value: string) => void;
	/** Why the BFF refused the last save's password, if it did. */
	failure?: PasswordFailure;
	/** Absent on a read-only entry. */
	onSave?: () => void;
	onDelete?: () => void;
	deleting: boolean;
	onToggleHidden?: () => void;
	hiding: boolean;
	error?: string | null;
}

/** The hero banner, dimmed into the page behind the header. Gone when it fails to load. */
const Backdrop: FC<{ src: string | null | undefined }> = ({ src }) => {
	const [failed, setFailed] = useState(false);
	if (!src || failed) return null;
	return (
		<div
			aria-hidden
			className="pointer-events-none absolute inset-x-0 top-0 -z-10 h-56 overflow-hidden rounded-xl"
		>
			<img
				src={src}
				alt=""
				className="size-full object-cover opacity-30"
				onError={() => setFailed(true)}
			/>
			<div className="absolute inset-0 bg-gradient-to-b from-background/40 to-background" />
		</div>
	);
};

/** Portrait, then whatever art there is, then the brand mark. */
const Poster: FC<{ entry: OperatorGameEntry | null }> = ({ entry }) => {
	const [failed, setFailed] = useState<Record<string, boolean>>({});
	const src = [entry?.art.portrait, entry?.art.header].find(
		(u): u is string => !!u && !failed[u],
	);
	return (
		<div className="flex aspect-[2/3] w-24 shrink-0 items-center justify-center overflow-hidden rounded-lg bg-muted text-muted-foreground shadow-lg ring-1 ring-border @md:w-36">
			{src ? (
				<img
					src={src}
					alt=""
					className="size-full object-cover"
					onError={() => setFailed((prev) => ({ ...prev, [src]: true }))}
				/>
			) : (
				<div className="w-full max-w-12 p-2 [&>svg]:size-full">
					<LauncherIcon icon={entry?.icon} />
				</div>
			)}
		</div>
	);
};

export const EntryHeader: FC<EntryHeaderProps> = ({
	entry,
	title,
	storeName,
	managedBy,
	dirty,
	saving,
	gated,
	password,
	onPassword,
	failure = null,
	onSave,
	onDelete,
	deleting,
	onToggleHidden,
	hiding,
	error,
}) => {
	const hidden = entry?.hidden === true;
	const creating = entry === null;
	const blocked = !dirty || saving || !title.trim() || (gated && !password);
	return (
		<div className="@container relative flex flex-col gap-4">
			<Backdrop src={entry?.art.hero} />
			<Link
				to="/library"
				className="inline-flex w-fit items-center gap-1 text-sm text-muted-foreground hover:text-foreground"
			>
				<ArrowLeft className="size-3.5" />
				{m.library_title()}
			</Link>
			<div className="flex flex-wrap items-end gap-4">
				<Poster entry={entry} />
				<div className="min-w-0 flex-1 space-y-2">
					<h1 className="break-words text-2xl font-semibold">
						{title.trim() || (creating ? m.library_add_title() : entry?.title)}
					</h1>
					<div className="flex flex-wrap gap-1">
						{storeName && <Badge variant="outline">{storeName}</Badge>}
						{entry?.platform && entry.platform.toUpperCase() !== "PC" && (
							<Badge variant="outline">{entry.platform}</Badge>
						)}
						{managedBy && (
							<Badge variant="secondary">
								{m.library_entry_managed_by({ source: managedBy })}
							</Badge>
						)}
						{hidden && (
							<Badge variant="secondary">{m.library_hidden_badge()}</Badge>
						)}
					</div>
				</div>
				<div className="flex w-full flex-wrap items-center gap-2 @lg:w-auto">
					{onToggleHidden && (
						<Button
							variant="outline"
							size="sm"
							aria-pressed={hidden}
							disabled={hiding}
							onClick={onToggleHidden}
						>
							{hidden ? (
								<Eye className="size-4" />
							) : (
								<EyeOff className="size-4" />
							)}
							{hidden ? m.library_unhide_action() : m.library_hide_action()}
						</Button>
					)}
					{onDelete && (
						<Button
							variant="outline"
							size="sm"
							disabled={deleting}
							onClick={onDelete}
						>
							<Trash2 className="size-4 text-destructive" />
							{m.library_delete()}
						</Button>
					)}
					{onSave && (
						<Button size="sm" disabled={blocked} onClick={onSave}>
							{creating ? m.library_create() : m.library_save()}
						</Button>
					)}
				</div>
			</div>
			{managedBy && (
				<p className="text-sm text-muted-foreground">
					{m.library_entry_managed_note({ source: managedBy })}
				</p>
			)}
			{/* Saving a command, or a row that carries prep, runs code as the host user: the
			    console password is asked for exactly when the BFF gate applies. */}
			{onSave && gated && (
				<div className="max-w-sm">
					<PasswordConfirmField
						id="entry-password"
						value={password}
						onChange={onPassword}
						failure={failure}
						help={m.library_field_password_help()}
					/>
				</div>
			)}
			{error && (
				<p
					role="alert"
					className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive"
				>
					{error}
				</p>
			)}
			{/* Phones: the header's Save scrolls away on a long tab, so a dirty page keeps one in
			    reach above the bottom nav. */}
			{onSave && dirty && (
				<div className="fixed inset-x-4 bottom-20 z-30 flex items-center justify-between gap-3 rounded-xl border bg-card/95 px-4 py-3 shadow-lg backdrop-blur sm:hidden">
					<span className="text-sm text-muted-foreground">
						{gated && !password
							? m.library_entry_password_first()
							: m.library_entry_unsaved()}
					</span>
					<Button size="sm" disabled={blocked} onClick={onSave}>
						{creating ? m.library_create() : m.library_save()}
					</Button>
				</div>
			)}
		</div>
	);
};
