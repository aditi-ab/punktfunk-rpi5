#!/usr/bin/env bun
// `punktfunk-scripting` — run the operator's scripts and punktfunk-plugin-* packages under
// supervision (see ./runner.ts). SIGINT/SIGTERM interrupt the whole tree structurally, so
// every plugin's finalizers run before exit (the systemd-stop story).
//
//   bun src/runner-cli.ts [--scripts DIR] [--plugins DIR] [--list]
import { Effect, Fiber } from "effect";
import { discoverUnits, runner } from "./runner.js";

const arg = (flag: string): string | undefined => {
	const i = process.argv.indexOf(flag);
	return i >= 0 ? process.argv[i + 1] : undefined;
};

const options = {
	scriptsDir: arg("--scripts"),
	pluginsDir: arg("--plugins"),
};

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
