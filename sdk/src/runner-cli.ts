#!/usr/bin/env bun
// `punktfunk-scripting` — the plugin/script runner AND the `punktfunk-host plugins …` package ops.
//
// With NO subcommand it RUNS the runner: discover the operator's scripts + punktfunk-plugin-*
// packages and supervise them (see ./runner.ts). SIGINT/SIGTERM interrupt the whole tree
// structurally, so every plugin's finalizers run before exit (the systemd-stop story). This bare
// form is what the systemd unit / Windows scheduled task launch — do not change its behavior.
//
// With a subcommand it manages plugin packages (the host CLI forwards `punktfunk-host plugins …`
// here):
//   add <name…>      install first-party plugins (playnite, rom-manager); anything resolving on
//                    the PUBLIC npm registry (punktfunk-plugin-*, foreign scopes) additionally
//                    needs --allow-public-registry
//   remove <name…>   uninstall
//   list             list installed plugin packages
//
//   bun src/runner-cli.ts [--scripts DIR] [--plugins DIR] [--list]   (run the runner)
//   bun src/runner-cli.ts add playnite [--plugins DIR]               (package ops)
//
// Package-op flags: --exact pins the resolved version instead of a caret range, and
// --registry @scope=https://… maps a scope to its registry in bunfig.toml. Both exist for the
// plugin store (crates/punktfunk-host/src/store), which installs one reviewed version of a
// package that may live on somebody else's registry — but they are ordinary CLI flags too.
import { Effect, Fiber } from "effect";
import { addPlugins, listInstalled, removePlugins } from "./plugins.js";
import { discoverUnits, runner } from "./runner.js";

const arg = (flag: string): string | undefined => {
	const i = process.argv.indexOf(flag);
	return i >= 0 ? process.argv[i + 1] : undefined;
};

/** Every value of a repeatable flag (`--registry a=b --registry c=d`). */
const argAll = (flag: string): string[] => {
	const out: string[] = [];
	for (let i = 0; i < process.argv.length; i++) {
		if (process.argv[i] === flag && process.argv[i + 1] !== undefined) out.push(process.argv[++i]);
	}
	return out;
};

/** Flags that take a value — skipped (with their value) when collecting positionals. */
const VALUE_FLAGS = ["--plugins", "--scripts", "--registry"];

const options = {
	scriptsDir: arg("--scripts"),
	pluginsDir: arg("--plugins"),
};

/**
 * `--registry @scope=https://registry/` → `{ "@scope": "https://registry/" }`. The plugin store
 * passes one per catalog entry so a third-party package's scope resolves to *its* registry rather
 * than the public npm default.
 */
const parseRegistries = (): Record<string, string> => {
	const out: Record<string, string> = {};
	for (const spec of argAll("--registry")) {
		const eq = spec.indexOf("=");
		if (eq <= 0) {
			console.error(`[plugins] ignoring malformed --registry '${spec}' (expected @scope=URL)`);
			continue;
		}
		const scope = spec.slice(0, eq).trim();
		const url = spec.slice(eq + 1).trim();
		if (!scope.startsWith("@") || !url.startsWith("https://")) {
			console.error(
				`[plugins] ignoring --registry '${spec}' (scope must start with @, url with https://)`,
			);
			continue;
		}
		out[scope] = url;
	}
	return out;
};

// Positional plugin names after the subcommand (argv: [bun, script, <cmd>, …]). Skip flags and the
// value of `--plugins`/`--scripts` wherever they appear, so ordering doesn't matter.
const positionals = (): string[] => {
	const out: string[] = [];
	for (let i = 3; i < process.argv.length; i++) {
		const a = process.argv[i];
		if (VALUE_FLAGS.includes(a)) {
			i++; // skip its value too
			continue;
		}
		if (a.startsWith("-")) continue;
		out.push(a);
	}
	return out;
};

const pkgOpts = {
	dir: options.pluginsDir,
	// Opt-in for names that resolve on the public npm registry (supply-chain gate in
	// plugins.ts::resolvePackage). Boolean flag, so positionals() skips it on its own.
	allowPublicRegistry: process.argv.includes("--allow-public-registry"),
	// Pin the exact version in package.json instead of a caret range — the plugin store installs
	// one reviewed version and must not let a later `bun install` drift off it.
	exact: process.argv.includes("--exact"),
	registries: parseRegistries(),
};

const runPkgOp = (
	op: (names: string[], o: typeof pkgOpts) => void,
	verb: string,
): never => {
	const names = positionals();
	if (names.length === 0) {
		console.error(
			`usage: punktfunk-host plugins ${verb} <name…>  (e.g. playnite, rom-manager)`,
		);
		process.exit(2);
	}
	try {
		op(names, pkgOpts);
		process.exit(0);
	} catch (e) {
		console.error(`[plugins] ${e instanceof Error ? e.message : e}`);
		process.exit(1);
	}
};

switch (process.argv[2]) {
	case "add":
		runPkgOp(addPlugins, "add");
		break;
	case "remove":
	case "rm":
	case "uninstall":
		runPkgOp(removePlugins, "remove");
		break;
	case "list":
	case "ls": {
		const installed = listInstalled(options.pluginsDir);
		if (installed.length === 0) {
			console.log("No plugins installed.");
		} else {
			for (const p of installed) {
				console.log(p.version ? `${p.pkg}\t${p.version}` : p.pkg);
			}
		}
		process.exit(0);
	}
}

// ---- run the runner (default; --list keeps the legacy unit-listing behavior) ------------------
if (process.argv.includes("--list")) {
	for (const u of discoverUnits(options)) console.log(`${u.name}\t${u.file}`);
	process.exit(0);
}

const fiber = Effect.runFork(runner(options));
let stopping = false;
const shutdown = (signal: string) => {
	if (stopping) return process.exit(1); // second signal = get out now
	stopping = true;
	console.log(`${new Date().toISOString()} [runner] ${signal} — interrupting units…`);
	void Effect.runPromise(Fiber.interrupt(fiber)).finally(() => process.exit(0));
};
process.on("SIGINT", () => shutdown("SIGINT"));
process.on("SIGTERM", () => shutdown("SIGTERM"));

await Effect.runPromise(Fiber.await(fiber));
