// The Omarchy theme, if this box has one.
//
// Omarchy renders every registered `~/.config/omarchy/themed/*.tpl` from the active theme's
// semantic `colors.toml` on each `omarchy-theme-set`, dropping the result in
// `~/.local/state/omarchy/current/theme/`. We register `punktfunk.json.tpl` (installed by
// `punktfunk-omarchy setup`, opt-in), so the file below is the theme expressed in exactly the four
// values the console can act on.
//
// Deliberately a FILE read and not an integration: there is no Omarchy API to call, the host learns
// nothing, and a box that never opted in simply has no file — which is why every failure here is
// "no theme", never an error. The console's own palette is the fallback and always was.
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export interface OmarchyTheme {
	/** `light` | `dark` — drives the `.dark` class the whole palette keys off. */
	mode: "light" | "dark";
	/** The theme's accent, as a CSS colour. Mapped onto `--pf-brand`, which `--primary`,
	 *  `--accent` and `--ring` all derive from, so one value re-tints the console. */
	accent: string;
}

/** Where Omarchy renders our template. `XDG_STATE_HOME` first, because that is what the spec says
 * and what a non-default setup uses; `~/.local/state` is the default it falls back to. */
function themePath(): string {
	const state =
		process.env.XDG_STATE_HOME?.trim() || join(homedir(), ".local", "state");
	return join(state, "omarchy", "current", "theme", "punktfunk.json");
}

/** A CSS colour we are willing to inline into a style attribute.
 *
 * This is the security-relevant line in the file: the value reaches the DOM, so anything that could
 * close the declaration and start another one is refused. Hex and the common functional notations
 * cover every theme Omarchy ships; anything else falls back to the console's own brand rather than
 * being sanitised into something half-right. */
function isSafeColor(v: unknown): v is string {
	return (
		typeof v === "string" &&
		v.length <= 64 &&
		/^(#[0-9a-fA-F]{3,8}|(rgb|rgba|hsl|hsla|oklch|oklab)\([0-9a-zA-Z.,%/\s-]+\))$/.test(
			v.trim(),
		)
	);
}

/**
 * The active Omarchy theme, or `null` when this box has none — which is every box that is not
 * Omarchy, and every Omarchy box whose operator did not opt in.
 *
 * Read per request rather than cached: `omarchy-theme-set` rewrites the file whenever the user
 * changes theme, and a console that only noticed at startup would be wrong until it restarted.
 * It is one small local read behind an already-authenticated route.
 */
export function omarchyTheme(): OmarchyTheme | null {
	let raw: string;
	try {
		raw = readFileSync(themePath(), "utf8");
	} catch {
		return null; // no file: not Omarchy, or not opted in
	}
	try {
		const parsed = JSON.parse(raw) as Record<string, unknown>;
		const mode = parsed.mode === "light" ? "light" : "dark";
		// An unrendered template still contains its `{{ accent }}` placeholder — that is not a
		// colour, and `isSafeColor` is what stops it reaching the page as one.
		if (!isSafeColor(parsed.accent)) return null;
		return { mode, accent: parsed.accent.trim() };
	} catch {
		return null; // half-written during a theme switch, or hand-edited into invalid JSON
	}
}
