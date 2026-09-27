// Unit discovery: the operator's loose scripts plus the plugin packages they installed.
import * as fs from "node:fs";
import * as path from "node:path";
import { configDir } from "./config.js";
import { fileIsSafe } from "./file-trust.js";
import type { RunnerOptions, Unit } from "./runner.js";
import { defaultLog, type LogSink } from "./runner-log.js";
import { readManifest } from "./sandbox.js";

const SCRIPT_EXTENSIONS = new Set([".ts", ".js", ".mjs", ".mts", ".cjs"]);

/** Enumerate the operator's units: loose scripts plus installed plugin packages. */
export const discoverUnits = (
	options: RunnerOptions = {},
	log: LogSink = options.log ?? defaultLog,
): Unit[] => {
	const units: Unit[] = [];
	const scriptsDir = options.scriptsDir ?? path.join(configDir(), "scripts");
	const pluginsDir = options.pluginsDir ?? path.join(configDir(), "plugins");
	try {
		for (const entry of fs.readdirSync(scriptsDir).sort()) {
			const file = path.join(scriptsDir, entry);
			if (!SCRIPT_EXTENSIONS.has(path.extname(entry))) continue;
			if (!fs.statSync(file).isFile()) continue;
			if (!fileIsSafe(file, log)) continue;
			units.push({ name: path.basename(entry, path.extname(entry)), file });
		}
	} catch {
		// no scripts dir — fine
	}
	const modules = path.join(pluginsDir, "node_modules");
	// The packages the operator actually installed — `bun add` records them as the plugins dir's
	// own `dependencies`. This is what separates a plugin from a plugin's LIBRARY:
	// `@punktfunk/plugin-kit` matches the `plugin-*` naming convention exactly but arrives as a
	// transitive dependency of every kit-built plugin, and running it as a unit is nonsense.
	// `undefined` only when there is no readable `package.json` at all — a hand-assembled tree then
	// falls back to the naming convention rather than discovering nothing. A package.json with no
	// `dependencies` key yields an EMPTY set, not `undefined`: `bun remove` drops the key when the
	// last plugin goes, and orphaned transitive packages can outlive it, so falling back there
	// would start running a plugin's library the moment you uninstall the last real plugin.
	let topLevel: Set<string> | undefined;
	try {
		const root = JSON.parse(
			fs.readFileSync(path.join(pluginsDir, "package.json"), "utf8"),
		) as { dependencies?: Record<string, string> };
		topLevel = new Set(Object.keys(root.dependencies ?? {}));
	} catch {
		// no package.json — fall back to the convention
	}
	// Read a plugin package's manifest (`module`/`main` entry) and add it as a unit.
	const addPlugin = (dir: string, name: string): void => {
		if (topLevel && !topLevel.has(name)) return; // a dependency, not an installed plugin
		try {
			const manifest = JSON.parse(
				fs.readFileSync(path.join(dir, "package.json"), "utf8"),
			) as { main?: string; module?: string };
			const rel = manifest.module ?? manifest.main ?? "index.js";
			const file = path.join(dir, rel);
			if (!fileIsSafe(file, log)) return;
			const declared = readManifest(dir);
			units.push({
				name,
				file,
				packageDir: dir,
				...(declared ? { manifest: declared } : {}),
			});
		} catch (e) {
			log(`[runner] skipping ${name}: unreadable package.json (${e})`, "warn");
		}
	};
	try {
		for (const pkg of fs.readdirSync(modules).sort()) {
			// Unscoped convention: `punktfunk-plugin-*`.
			if (pkg.startsWith("punktfunk-plugin-")) {
				addPlugin(path.join(modules, pkg), pkg);
				continue;
			}
			// Scoped convention: `<any scope>/plugin-*`. A scoped name resolves cleanly from a
			// registry scope-map, so a plugin can depend on `@punktfunk/host` + `effect` as shared
			// (hoisted) deps rather than bundling its own copy of each.
			//
			// ANY scope, not just `@punktfunk`: the plugin store requires catalog entries to be
			// scoped precisely so the scope can map to that entry's registry, so a third-party
			// plugin necessarily arrives as `@their-scope/plugin-*`. Limiting discovery to the
			// first-party scope would let such a plugin install and then never run.
			if (pkg.startsWith("@")) {
				try {
					for (const scoped of fs.readdirSync(path.join(modules, pkg)).sort()) {
						if (scoped.startsWith("plugin-")) {
							addPlugin(path.join(modules, pkg, scoped), `${pkg}/${scoped}`);
						}
					}
				} catch {
					// not a readable scope dir — fine
				}
			}
		}
	} catch {
		// no plugins dir — fine
	}
	return units;
};
