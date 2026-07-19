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
//   add <name…>      install first-party (playnite, rom-manager) or punktfunk-plugin-* packages
//   remove <name…>   uninstall
//   list             list installed plugin packages
//
//   bun src/runner-cli.ts [--scripts DIR] [--plugins DIR] [--list]   (run the runner)
//   bun src/runner-cli.ts add playnite [--plugins DIR]               (package ops)
import { Effect, Fiber } from "effect";
import { addPlugins, listInstalled, removePlugins } from "./plugins.js";
import { discoverUnits, runner } from "./runner.js";

const arg = (flag: string): string | undefined => {
	const i = process.argv.indexOf(flag);
	return i >= 0 ? process.argv[i + 1] : undefined;
};

const options = {
	scriptsDir: arg("--scripts"),
	pluginsDir: arg("--plugins"),
};

// Positional plugin names after the subcommand (argv: [bun, script, <cmd>, …]). Skip flags and the
// value of `--plugins`/`--scripts` wherever they appear, so ordering doesn't matter.
const positionals = (): string[] => {
	const out: string[] = [];
	for (let i = 3; i < process.argv.length; i++) {
		const a = process.argv[i];
		if (a === "--plugins" || a === "--scripts") {
			i++; // skip its value too
			continue;
		}
		if (a.startsWith("-")) continue;
		out.push(a);
	}
	return out;
};

const pkgOpts = { dir: options.pluginsDir };

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
