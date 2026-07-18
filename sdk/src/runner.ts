// The managed script/plugin runner (RFC §8, M5) — what the `punktfunk-scripting` package runs:
// discover the operator's units, supervise them as Effect fibers, shut down structurally.
//
// Units:
// - **Plugins** — a file whose default export is a [`PluginDef`] (`definePlugin`), from the
//   scripts dir or an installed `punktfunk-plugin-*` package. Supervised: a failure restarts
//   it with capped exponential backoff; a clean return completes it. The Effect `main` shape
//   runs with the `PunktfunkHost` layer provided and is interrupted STRUCTURALLY on shutdown
//   (scoped finalizers run — release the preset, deregister cleanly); the async-fn shape gets
//   a connected facade client whose close is guaranteed by the same scope.
// - **Bare scripts** — any other `.ts`/`.js` file in the scripts dir: importing it IS the run
//   (top-level await). One-shot: completion logs, failure logs — no restart (a bare script's
//   background work is invisible to supervision; export a plugin to be supervised).
//
// Trust model (RFC §9.4): a unit is code the operator chose to run — no sandbox is pretended.
// The same sshd rule as hooks applies: a world-writable unit file is refused loudly.
import {
	Cause,
	Duration,
	Effect,
	Schedule,
} from "effect";
import * as fs from "node:fs";
import * as path from "node:path";
import { pathToFileURL } from "node:url";
import { layer as hostLayer, PunktfunkHost } from "./client.js";
import { type ConnectOptions, configDir } from "./config.js";
import { connect, type PluginDef } from "./index.js";

export interface RunnerOptions {
	/** Where loose scripts live. Default `<config_dir>/scripts`. */
	scriptsDir?: string;
	/**
	 * Where plugin packages are installed (`<pluginsDir>/node_modules/punktfunk-plugin-*`,
	 * i.e. the operator runs `bun add punktfunk-plugin-x` there). Default `<config_dir>/plugins`.
	 */
	pluginsDir?: string;
	/** Connection overrides handed to every unit's client/layer. */
	connect?: ConnectOptions;
	/** Restart backoff base (test seam). Default 1 s, capped at 60 s, jittered. */
	restartBase?: Duration.Input;
	/** Line sink. Default: stamped stdout. */
	log?: (line: string) => void;
}

export interface Unit {
	/** Display name: the file stem, or the plugin package name. */
	name: string;
	/** Absolute path of the module to import. */
	file: string;
}

const defaultLog = (line: string) =>
	console.log(`${new Date().toISOString()} ${line}`);

/** The sshd rule (RFC §9.1/§9.4): refuse group/world-writable unit files, loudly. */
const fileIsSafe = (file: string, log: (l: string) => void): boolean => {
	if (process.platform === "win32") return true; // config dir is DACL'd; ACL check is a follow-up
	try {
		const mode = fs.statSync(file).mode & 0o022;
		if (mode !== 0) {
			log(
				`[runner] REFUSING ${file} — group/world-writable (chmod go-w it first)`,
			);
			return false;
		}
	} catch {
		return false;
	}
	return true;
};

const SCRIPT_EXTENSIONS = new Set([".ts", ".js", ".mjs", ".mts", ".cjs"]);

/** Enumerate the operator's units: loose scripts plus installed plugin packages. */
export const discoverUnits = (
	options: RunnerOptions = {},
	log: (l: string) => void = options.log ?? defaultLog,
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
	// Read a plugin package's manifest (`module`/`main` entry) and add it as a unit.
	const addPlugin = (dir: string, name: string): void => {
		try {
			const manifest = JSON.parse(
				fs.readFileSync(path.join(dir, "package.json"), "utf8"),
			) as { main?: string; module?: string };
			const rel = manifest.module ?? manifest.main ?? "index.js";
			const file = path.join(dir, rel);
			if (!fileIsSafe(file, log)) return;
			units.push({ name, file });
		} catch (e) {
			log(`[runner] skipping ${name}: unreadable package.json (${e})`);
		}
	};
	try {
		for (const pkg of fs.readdirSync(modules).sort()) {
			// Unscoped convention: `punktfunk-plugin-*`.
			if (pkg.startsWith("punktfunk-plugin-")) {
				addPlugin(path.join(modules, pkg), pkg);
				continue;
			}
			// Scoped convention: `@punktfunk/plugin-*` (first-party). A scoped name resolves cleanly
			// from a single registry scope-map, so a plugin can depend on `@punktfunk/host` + `effect`
			// as shared (hoisted) deps rather than bundling its own copy of each.
			if (pkg === "@punktfunk") {
				try {
					for (const scoped of fs.readdirSync(path.join(modules, pkg)).sort()) {
						if (scoped.startsWith("plugin-")) {
							addPlugin(path.join(modules, pkg, scoped), `${pkg}/${scoped}`);
						}
					}
				} catch {
					// no @punktfunk scope dir — fine
				}
			}
		}
	} catch {
		// no plugins dir — fine
	}
	return units;
};

const isPluginDef = (v: unknown): v is PluginDef =>
	typeof v === "object" &&
	v !== null &&
	typeof (v as PluginDef).name === "string" &&
	(v as PluginDef).main !== undefined;

/** One attempt at a unit: import (cache-busted per attempt) and run whatever it exports. */
const attemptUnit = (
	unit: Unit,
	attempt: number,
	options: RunnerOptions,
	log: (l: string) => void,
): Effect.Effect<"plugin" | "script", unknown> =>
	Effect.gen(function* () {
		const mod = (yield* Effect.tryPromise(
			() => import(`${pathToFileURL(unit.file).href}?attempt=${attempt}`),
		)) as { default?: unknown };
		if (!isPluginDef(mod.default)) {
			return "script" as const; // the import WAS the run (top-level await)
		}
		const def = mod.default;
		if (Effect.isEffect(def.main)) {
			// The well-behaved shape: interruption reaches it structurally, its scoped
			// finalizers run on shutdown.
			yield* (def.main as Effect.Effect<unknown, unknown, PunktfunkHost>).pipe(
				Effect.provide(hostLayer(options.connect)),
			);
		} else {
			// The simple shape: a facade client whose close is guaranteed by the scope —
			// on completion, failure, OR interruption (shutdown).
			const main = def.main as (pf: unknown) => Promise<unknown> | unknown;
			yield* Effect.scoped(
				Effect.gen(function* () {
					const pf = yield* Effect.acquireRelease(
						Effect.tryPromise(() => connect(options.connect)),
						(client) => Effect.sync(() => client.close()),
					);
					yield* Effect.tryPromise(async () => await main(pf));
				}),
			);
		}
		return "plugin" as const;
	});

/**
 * A unit under supervision: plugins restart on failure (capped exponential backoff, jittered);
 * a clean completion ends the unit; bare scripts are one-shot either way. Never fails the
 * runner — every outcome is logged.
 */
export const superviseUnit = (
	unit: Unit,
	options: RunnerOptions = {},
): Effect.Effect<void> => {
	const log = options.log ?? defaultLog;
	// Exponential backoff, capped at 60 s (min-delay of the two schedules), then jittered.
	// (v4 replaced `Schedule.union` with the array-form `Schedule.min`.)
	const restart = Schedule.min([
		Schedule.exponential(options.restartBase ?? "1 second"),
		Schedule.spaced("60 seconds"),
	]).pipe(Schedule.jittered);
	let attempt = 0;
	const once = Effect.suspend(() => {
		attempt += 1;
		if (attempt > 1) log(`[${unit.name}] restarting (attempt ${attempt})`);
		return attemptUnit(unit, attempt, options, log);
	});
	return once.pipe(
		Effect.tap((kind) =>
			Effect.sync(() =>
				log(
					kind === "script"
						? `[${unit.name}] script completed`
						: `[${unit.name}] plugin completed`,
				),
			),
		),
		Effect.tapCause((cause) =>
			Effect.sync(() =>
				log(`[${unit.name}] failed: ${Cause.pretty(cause).split("\n")[0]}`),
			),
		),
		Effect.retry(restart),
		Effect.catchCause((cause) =>
			// A retry schedule that gives up (it doesn't, but stay total) — log and end.
			Effect.sync(() => log(`[${unit.name}] gave up: ${Cause.pretty(cause)}`)),
		),
		Effect.asVoid,
	);
};

/**
 * The runner: discover units, supervise each as a fiber, run until interrupted — at which
 * point every unit is interrupted STRUCTURALLY (scoped finalizers run: facade clients close,
 * Effect plugins release what they acquired).
 */
export const runner = (options: RunnerOptions = {}): Effect.Effect<void> => {
	const log = options.log ?? defaultLog;
	return Effect.scoped(
		Effect.gen(function* () {
			const units = discoverUnits(options, log);
			if (units.length === 0) {
				log(
					"[runner] nothing to run — add scripts to the scripts dir or install punktfunk-plugin-* packages",
				);
			}
			for (const unit of units) {
				log(`[runner] starting ${unit.name} (${unit.file})`);
				yield* Effect.forkScoped(superviseUnit(unit, options));
			}
			yield* Effect.never; // interruption (shutdown) collapses the scope → all units
		}),
	);
};
