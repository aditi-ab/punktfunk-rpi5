// `punktfunk-host plugins …` package operations, run on the vendored bun. The host CLI forwards
// add/remove/list here (crates/punktfunk-host/src/plugins.rs) and the runner-cli exposes them as
// subcommands. Everything a plugin needs to be installed — the plugins dir, the `@punktfunk`
// registry scope in bunfig.toml, and the right bun — is handled here so the operator types one line
// instead of the old create-dir / write-bunfig / `bun add` ritual.
import * as fs from "node:fs";
import * as path from "node:path";
import { configDir } from "./config.js";

/** The `@punktfunk` package registry (Gitea's npm registry for the `unom` org). */
export const REGISTRY = "https://git.unom.io/api/packages/unom/npm/";

/** Where plugin packages install: `<config_dir>/plugins` (matches runner.ts discovery). */
export const pluginsDirDefault = (): string => path.join(configDir(), "plugins");

export interface ResolveOptions {
	/**
	 * Allow names that resolve on the PUBLIC npm registry (unscoped `punktfunk-plugin-*`, foreign
	 * scopes, arbitrary paths). Off by default: only the `@punktfunk` scope — pinned to the Gitea
	 * registry by [`ensureBunfig`] — installs without it, so a typo or a squatted look-alike
	 * package can't silently pull operator-privileged code from npmjs.org (the CLI flag is
	 * `--allow-public-registry`).
	 */
	allowPublicRegistry?: boolean;
}

/**
 * Resolve a friendly plugin name to its npm package. A bare first-party name maps into the
 * `@punktfunk` scope (`playnite` → `@punktfunk/plugin-playnite`, `rom-manager` →
 * `@punktfunk/plugin-rom-manager`); an `@punktfunk/…` name is used verbatim. Anything else —
 * the unscoped `punktfunk-plugin-…` convention, foreign scopes, registry paths — resolves on
 * the public registry and is refused unless [`ResolveOptions.allowPublicRegistry`] is set.
 */
export const resolvePackage = (
	name: string,
	opts: ResolveOptions = {},
): string => {
	const n = name.trim();
	if (!n) throw new Error("empty plugin name");
	if (!n.startsWith("@") && !n.includes("/") && !n.startsWith("punktfunk-plugin-")) {
		return `@punktfunk/plugin-${n}`; // bare first-party name
	}
	if (n.startsWith("@punktfunk/")) return n; // first-party scope, pinned to our registry
	if (!opts.allowPublicRegistry) {
		throw new Error(
			`'${n}' would install from the PUBLIC npm registry, not Punktfunk's. Plugins run ` +
				"with operator privileges - install only code you trust. If you mean it, re-run " +
				"with --allow-public-registry.",
		);
	}
	return n;
};

/** Does this resolved package name install from Punktfunk's own (Gitea) registry? */
const isFirstParty = (pkg: string): boolean => pkg.startsWith("@punktfunk/");

/** Create the plugins dir (and parents) if needed. On Windows the ACL lockdown is the host's job. */
export const ensurePluginsDir = (dir = pluginsDirDefault()): string => {
	fs.mkdirSync(dir, { recursive: true });
	return dir;
};

/**
 * Ensure `<dir>/bunfig.toml` points the `@punktfunk` scope at the registry so `bun add` resolves
 * first-party plugins. Idempotent: a file already mapping the scope is left untouched; an existing
 * bunfig with an `[install.scopes]` table gets our line inserted under it; anything else appends a
 * fresh table.
 */
export const ensureBunfig = (dir = pluginsDirDefault()): void => {
	const file = path.join(dir, "bunfig.toml");
	const scopeLine = `"@punktfunk" = "${REGISTRY}"`;
	let existing = "";
	try {
		existing = fs.readFileSync(file, "utf8");
	} catch {
		// no bunfig yet — write a fresh one below
	}
	if (existing.includes("@punktfunk") && existing.includes(REGISTRY)) return; // already wired

	const table = `[install.scopes]\n${scopeLine}\n`;
	if (!existing.trim()) {
		fs.writeFileSync(file, table);
	} else if (/^\[install\.scopes\][^\n]*$/m.test(existing)) {
		// Insert our scope line right after the existing table header.
		fs.writeFileSync(
			file,
			existing.replace(/^\[install\.scopes\][^\n]*$/m, (m) => `${m}\n${scopeLine}`),
		);
	} else {
		const sep = existing.endsWith("\n") ? "" : "\n";
		fs.writeFileSync(file, `${existing}${sep}\n${table}`);
	}
};

export interface PkgOpts extends ResolveOptions {
	/** Plugins dir. Default `<config_dir>/plugins`. */
	dir?: string;
	/** Line sink for progress. Default stdout. */
	log?: (line: string) => void;
}

/** Run `bun add`/`bun remove` in the plugins dir on the current (vendored) bun. */
const runBun = (action: "add" | "remove", pkgs: string[], opts: PkgOpts): void => {
	const dir = opts.dir ?? pluginsDirDefault();
	const log = opts.log ?? ((l: string) => console.log(l));
	ensurePluginsDir(dir);
	if (action === "add") ensureBunfig(dir);
	log(`${action === "add" ? "installing" : "removing"} ${pkgs.join(", ")} in ${dir}`);
	// `process.execPath` is the bun running this file (the vendored one under the package), so a
	// system-wide bun on PATH is not required. Inherit stdio so `bun`'s progress reaches the user.
	const args = [process.execPath, action, ...pkgs];
	// Windows: install file COPIES, never bun's default hardlinks. A hardlinked file's canonical
	// path resolves into the installing admin's per-user bun cache
	// (C:\Users\<admin>\.bun\install\cache\…), which the de-privileged LocalService runner cannot
	// traverse — imports die with EPERM even though the plugins-dir DACL grants read (seen live
	// on-glass). copyfile keeps the plugins tree self-contained under %ProgramData%.
	if (action === "add" && process.platform === "win32") {
		args.push("--backend=copyfile");
	}
	const res = Bun.spawnSync(args, {
		cwd: dir,
		stdio: ["inherit", "inherit", "inherit"],
	});
	if (!res.success) {
		throw new Error(`bun ${action} exited ${res.exitCode ?? "?"} — see output above`);
	}
};

/** Install one or more plugins by friendly name or package. */
export const addPlugins = (names: string[], opts: PkgOpts = {}): void => {
	const pkgs = names.map((n) => resolvePackage(n, opts));
	const log = opts.log ?? ((l: string) => console.log(l));
	for (const pkg of pkgs.filter((p) => !isFirstParty(p))) {
		log(
			`[plugins] WARNING: ${pkg} installs from the public npm registry - it is not ` +
				"published by Punktfunk. It will run with operator privileges.",
		);
	}
	runBun("add", pkgs, opts);
};

/** Uninstall one or more plugins by friendly name or package. Removal is always safe — a name
 * never gates on the registry it once came from. */
export const removePlugins = (names: string[], opts: PkgOpts = {}): void =>
	runBun(
		"remove",
		names.map((n) => resolvePackage(n, { allowPublicRegistry: true })),
		opts,
	);

export interface InstalledPlugin {
	/** npm package name, e.g. `@punktfunk/plugin-playnite` or `punktfunk-plugin-foo`. */
	pkg: string;
	/** Installed version from the package's package.json, if readable. */
	version?: string;
}

/**
 * Enumerate installed plugin packages under `<dir>/node_modules` — both the scoped first-party
 * convention (`@punktfunk/plugin-*`) and the unscoped one (`punktfunk-plugin-*`). Mirrors the
 * discovery in runner.ts so `list` shows exactly what the runner would supervise.
 */
export const listInstalled = (dir = pluginsDirDefault()): InstalledPlugin[] => {
	const modules = path.join(dir, "node_modules");
	const out: InstalledPlugin[] = [];
	const versionOf = (pkgDir: string): string | undefined => {
		try {
			const m = JSON.parse(
				fs.readFileSync(path.join(pkgDir, "package.json"), "utf8"),
			) as { version?: string };
			return m.version;
		} catch {
			return undefined;
		}
	};
	let entries: string[];
	try {
		entries = fs.readdirSync(modules).sort();
	} catch {
		return out; // no plugins installed yet
	}
	for (const entry of entries) {
		if (entry.startsWith("punktfunk-plugin-")) {
			out.push({ pkg: entry, version: versionOf(path.join(modules, entry)) });
		} else if (entry === "@punktfunk") {
			let scoped: string[] = [];
			try {
				scoped = fs.readdirSync(path.join(modules, entry)).sort();
			} catch {
				scoped = [];
			}
			for (const s of scoped) {
				if (s.startsWith("plugin-")) {
					out.push({
						pkg: `${entry}/${s}`,
						version: versionOf(path.join(modules, entry, s)),
					});
				}
			}
		}
	}
	return out;
};
