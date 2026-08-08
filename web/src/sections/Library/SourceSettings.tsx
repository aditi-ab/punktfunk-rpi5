import { toast } from "@unom/ui/toast";
import { type FC, useEffect, useState } from "react";
import type { ScannerInfo } from "@/api/gen/model/scannerInfo";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
	Dialog,
	DialogContent,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { m } from "@/paraglide/messages";

/**
 * A library source's settings, rendered as a **generic form** from the plugin's own JSON Schema.
 *
 * The point (design D7, closing G8): a scanner plugin ships no SPA at all. It serves
 * `GET/PUT /__config` from the kit, and the console renders whatever schema comes back. The browser
 * never learns the plugin's port or secret — the console reads it server-side over loopback.
 *
 * That read goes through `/api/plugin-config/<id>` on the CONSOLE origin, not the `/plugin-ui/…`
 * proxy this used to call. Plugin UIs live on their own origin (2026-08-05 review H-3) and the
 * console origin now answers 404 for `/plugin-ui/**` by design, which broke this drawer for every
 * library plugin — it is the one consumer of that path that is not an iframe. What it needs is
 * DATA, not an embedded UI, so it gets JSON same-origin and no plugin markup ever reaches the
 * console origin.
 *
 * Fields the derivation can't express fall back to a raw JSON editor. That fallback is what bounds
 * the risk of the whole approach: worst case the drawer is a validated textarea, and the PUT still
 * validates by decode host-side either way.
 */
export const SourceSettingsDialog: FC<{
	source: ScannerInfo;
	onClose: () => void;
}> = ({ source, onClose }) => {
	const pluginId = source.provider ?? source.id;
	const [state, setState] = useState<
		| { tag: "loading" }
		| { tag: "error"; message: string }
		| { tag: "ready"; schema: JsonSchemaDoc | null; value: JsonObject }
	>({ tag: "loading" });
	const [raw, setRaw] = useState("");
	const [saving, setSaving] = useState(false);

	useEffect(() => {
		let cancelled = false;
		(async () => {
			try {
				const res = await fetch(`/api/plugin-config/${pluginId}`, {
					credentials: "same-origin",
				});
				if (!res.ok) throw new Error(`HTTP ${res.status}`);
				const body = (await res.json()) as {
					schema: JsonSchemaDoc | null;
					value: JsonObject | null;
				};
				if (cancelled) return;
				const value = body.value ?? {};
				setState({ tag: "ready", schema: body.schema, value });
				setRaw(JSON.stringify(value, null, 2));
			} catch (e) {
				if (!cancelled) {
					setState({ tag: "error", message: String(e) });
				}
			}
		})();
		return () => {
			cancelled = true;
		};
	}, [pluginId]);

	const save = async (value: JsonObject) => {
		setSaving(true);
		try {
			const res = await fetch(`/api/plugin-config/${pluginId}`, {
				method: "PUT",
				credentials: "same-origin",
				headers: { "content-type": "application/json" },
				body: JSON.stringify(value),
			});
			if (!res.ok) {
				const body = (await res.json().catch(() => null)) as {
					issue?: string;
				} | null;
				throw new Error(body?.issue ?? `HTTP ${res.status}`);
			}
			toast.success(m.library_source_settings_saved());
			onClose();
		} catch (e) {
			toast.error(m.library_source_settings_failed({ issue: String(e) }));
		} finally {
			setSaving(false);
		}
	};

	return (
		<Dialog open onOpenChange={(open) => !open && onClose()}>
			<DialogContent>
				<DialogHeader>
					<DialogTitle>
						{m.library_source_settings_title({ source: source.label })}
					</DialogTitle>
				</DialogHeader>
				{state.tag === "loading" && <Spinner />}
				{state.tag === "error" && (
					<p className="text-sm text-destructive">
						{m.library_source_settings_unreachable({ issue: state.message })}
					</p>
				)}
				{state.tag === "ready" && (
					<ConfigForm
						schema={state.schema}
						value={state.value}
						raw={raw}
						onRaw={setRaw}
						saving={saving}
						onSave={save}
					/>
				)}
			</DialogContent>
		</Dialog>
	);
};

type JsonObject = Record<string, unknown>;

interface JsonSchemaNode {
	type?: string;
	title?: string;
	description?: string;
	default?: unknown;
	enum?: string[];
	properties?: Record<string, JsonSchemaNode>;
	items?: JsonSchemaNode;
	allOf?: JsonSchemaNode[];
}

interface JsonSchemaDoc {
	schema?: JsonSchemaNode;
}

/**
 * Flatten a node's `allOf` branches into it. A *checked* schema (effect's `Schema.Int`, or anything
 * with `.check(...)`) nests its annotations and constraints there rather than at the top level, so
 * a form that only reads the top level silently loses every title and default on those fields.
 */
const flatten = (node: JsonSchemaNode): JsonSchemaNode =>
	(node.allOf ?? []).reduce<JsonSchemaNode>(
		(acc, branch) => ({ ...acc, ...branch }),
		{ ...node },
	);

/** Can this field be rendered as a real input? Anything else sends the whole form to the editor. */
const renderable = (node: JsonSchemaNode): boolean => {
	const n = flatten(node);
	if (n.enum) return true;
	if (n.type === "boolean" || n.type === "string") return true;
	if (n.type === "number" || n.type === "integer") return true;
	if (n.type === "array" && flatten(n.items ?? {}).type === "string")
		return true;
	if (n.type === "object" && n.properties) {
		return Object.values(n.properties).every(renderable);
	}
	return false;
};

const ConfigForm: FC<{
	schema: JsonSchemaDoc | null;
	value: JsonObject;
	raw: string;
	onRaw: (v: string) => void;
	saving: boolean;
	onSave: (value: JsonObject) => void;
}> = ({ schema, value, raw, onRaw, saving, onSave }) => {
	const [draft, setDraft] = useState<JsonObject>(value);
	const root = schema?.schema ? flatten(schema.schema) : undefined;
	const props = root?.properties;
	// Fall back to the JSON editor when there is no schema, or any field is a shape the generic
	// form can't express (a non-enum union, a $ref). Partial rendering would be worse than none:
	// a field silently missing from the form is a setting the operator cannot change.
	const canRender =
		props !== undefined && Object.values(props).every(renderable);

	if (!canRender) {
		return (
			<div className="space-y-3">
				<p className="text-xs text-muted-foreground">
					{m.library_source_settings_json_hint()}
				</p>
				<Textarea
					className="h-64 font-mono text-xs"
					value={raw}
					onChange={(e) => onRaw(e.target.value)}
					spellCheck={false}
				/>
				<Button
					disabled={saving}
					onClick={() => {
						try {
							onSave(JSON.parse(raw) as JsonObject);
						} catch (e) {
							toast.error(
								m.library_source_settings_failed({ issue: String(e) }),
							);
						}
					}}
				>
					{m.library_source_settings_save()}
				</Button>
			</div>
		);
	}

	return (
		<div className="space-y-4">
			{Object.entries(props).map(([key, rawNode]) => (
				<Field
					key={key}
					name={key}
					node={flatten(rawNode)}
					value={draft[key]}
					onChange={(v) => setDraft((d) => ({ ...d, [key]: v }))}
				/>
			))}
			<Button disabled={saving} onClick={() => onSave(draft)}>
				{m.library_source_settings_save()}
			</Button>
		</div>
	);
};

/** One schema field. `undefined` in the draft means "unset" — the file keeps its default out. */
const Field: FC<{
	name: string;
	node: JsonSchemaNode;
	value: unknown;
	onChange: (v: unknown) => void;
}> = ({ name, node, value, onChange }) => {
	const label = node.title ?? name;
	const id = `cfg-${name}`;

	if (node.type === "object" && node.properties) {
		const nested = (value ?? {}) as JsonObject;
		return (
			<fieldset className="space-y-3 rounded-lg border p-3">
				<legend className="px-1 text-sm font-medium">{label}</legend>
				{Object.entries(node.properties).map(([k, n]) => (
					<Field
						key={k}
						name={`${name}.${k}`}
						node={flatten(n)}
						value={nested[k]}
						onChange={(v) => onChange({ ...nested, [k]: v })}
					/>
				))}
			</fieldset>
		);
	}

	if (node.enum) {
		return (
			<div className="space-y-1">
				<Label htmlFor={id}>{label}</Label>
				<Select
					value={String(value ?? node.default ?? node.enum[0])}
					onValueChange={onChange}
				>
					<SelectTrigger id={id} size="sm">
						<SelectValue />
					</SelectTrigger>
					<SelectContent>
						{node.enum.map((opt) => (
							<SelectItem key={opt} value={opt}>
								{opt}
							</SelectItem>
						))}
					</SelectContent>
				</Select>
				{node.description && (
					<p className="text-xs text-muted-foreground">{node.description}</p>
				)}
			</div>
		);
	}

	if (node.type === "boolean") {
		const checked = (value ?? node.default ?? false) as boolean;
		return (
			<div className="space-y-1">
				<div className="flex items-center gap-2">
					<Checkbox
						id={id}
						checked={checked}
						onCheckedChange={(next) => onChange(next === true)}
					/>
					<Label htmlFor={id}>{label}</Label>
				</div>
				{node.description && (
					<p className="text-xs text-muted-foreground">{node.description}</p>
				)}
			</div>
		);
	}

	if (node.type === "array") {
		// One absolute path per line — the shape every "extra library folders" setting wants.
		const list = (value ?? node.default ?? []) as string[];
		return (
			<div className="space-y-1">
				<Label htmlFor={id}>{label}</Label>
				<Textarea
					id={id}
					className="h-24 font-mono text-xs"
					value={list.join("\n")}
					onChange={(e) =>
						onChange(
							e.target.value
								.split("\n")
								.map((s) => s.trim())
								.filter((s) => s !== ""),
						)
					}
				/>
				{node.description && (
					<p className="text-xs text-muted-foreground">{node.description}</p>
				)}
			</div>
		);
	}

	const numeric = node.type === "number" || node.type === "integer";
	return (
		<div className="space-y-1">
			<Label htmlFor={id}>{label}</Label>
			<Input
				id={id}
				type={numeric ? "number" : "text"}
				value={String(value ?? "")}
				placeholder={node.default != null ? String(node.default) : undefined}
				onChange={(e) => {
					const v = e.target.value;
					// An emptied field means "unset", which is NOT the same as zero or "" — it is what
					// keeps the operator's file free of a value they never chose.
					if (v === "") return onChange(undefined);
					onChange(numeric ? Number(v) : v);
				}}
			/>
			{node.description && (
				<p className="text-xs text-muted-foreground">{node.description}</p>
			)}
		</div>
	);
};
