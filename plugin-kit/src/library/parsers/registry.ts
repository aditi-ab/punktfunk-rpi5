// Windows registry reads by spawning `reg.exe query` — dependency-free, and (the part that
// matters) it works from the scripting runner's LocalService account.
//
// **HKLM only, by design.** The runner runs as `NT AUTHORITY\LocalService` on Windows, which has no
// user profile: HKCU is not the operator's hive there, it is LocalService's own — so a plugin that
// read HKCU would silently see an empty registry rather than the user's launcher config. Every
// launcher fact a scanner needs (Steam's InstallPath, GOG's game list) lives under HKLM
// `WOW6432Node` anyway. Asking for HKCU is a bug, so this refuses it outright.
import { spawnSync } from "node:child_process";

/** One `reg.exe query` value row. */
export interface RegValue {
	readonly name: string;
	/** `REG_SZ`, `REG_DWORD`, … */
	readonly type: string;
	readonly data: string;
}

const HKLM = "HKLM\\";

/** Is this a key path this module will touch? See the module docs on why HKLM only. */
export const validRegKey = (key: string): boolean =>
	key.startsWith(HKLM) &&
	key.length > HKLM.length &&
	key.length <= 260 &&
	!key.includes("..") &&
	// `reg.exe` takes the key as one argv element (no shell), but keep the charset tame anyway so a
	// malformed key can never turn into a switch.
	!key.startsWith("/") &&
	!/[\r\n\0"]/.test(key);

const run = (args: string[]): string | undefined => {
	if (process.platform !== "win32") return undefined;
	const r = spawnSync("reg.exe", args, {
		encoding: "utf8",
		windowsHide: true,
		// A registry read is instant; a hang means something is badly wrong and a scan must not
		// block on it forever.
		timeout: 10_000,
		maxBuffer: 4 * 1024 * 1024,
	});
	if (r.status !== 0 || typeof r.stdout !== "string") return undefined;
	return r.stdout;
};

/**
 * The values directly under one HKLM key. `[]` when the key is absent, unreadable, or this is not
 * Windows — a missing launcher is the normal case, never an error.
 */
export const regQueryValues = (key: string): RegValue[] => {
	if (!validRegKey(key)) return [];
	const out = run(["query", key]);
	if (out === undefined) return [];
	return parseRegQuery(out);
};

/** One named value under an HKLM key, or `undefined`. */
export const regQueryValue = (key: string, name: string): string | undefined =>
	regQueryValues(key).find((v) => v.name.toLowerCase() === name.toLowerCase())
		?.data;

/** The immediate SUBKEY paths under one HKLM key (GOG lists one subkey per installed game). */
export const regSubKeys = (key: string): string[] => {
	if (!validRegKey(key)) return [];
	const out = run(["query", key]);
	if (out === undefined) return [];
	const prefix = `${key.toLowerCase()}\\`;
	return out
		.split(/\r?\n/)
		.map((l) => l.trim())
		.filter((l) => l.toLowerCase().startsWith(prefix))
		.filter((l) => !l.slice(key.length + 1).includes("\\"));
};

/**
 * Parse `reg.exe query` output rows: `    <name>    <TYPE>    <data>`, separated by runs of
 * whitespace. Data may itself contain spaces (a path), so only the first two columns are split off.
 *
 * Exported for tests — the format is stable but this is exactly the kind of thing that quietly
 * breaks, and a plugin's tests can pin it without a Windows box.
 */
export const parseRegQuery = (stdout: string): RegValue[] => {
	const out: RegValue[] = [];
	for (const raw of stdout.split(/\r?\n/)) {
		// Value rows are indented; the key path header is not.
		if (!/^\s/.test(raw)) continue;
		const line = raw.trim();
		if (line === "") continue;
		const m = line.match(/^(.*?)\s{2,}(REG_[A-Z_]+)\s{2,}([\s\S]*)$/);
		if (!m) continue;
		out.push({ name: m[1], type: m[2], data: m[3] });
	}
	return out;
};
