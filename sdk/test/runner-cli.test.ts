// The shipped entry point's idle contract: `punktfunk-scripting` with nothing to run must PARK,
// not spin. `Effect.runFork` registers nothing with the event loop (the keep-alive lives in
// `Runtime.makeRunMain`, which runner-cli deliberately doesn't use), and a bun process whose only
// pending work is an unresolved promise busy-loops instead of blocking — so in the runner's own
// documented no-op state (no scripts, no plugins) it pinned a full core indefinitely until
// runner-cli started holding one idle handle. Measured against the real CLI rather than mocked:
// the regression is a property of the process, not of any function in it.
import { afterAll, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as path from "node:path";

const SDK = path.join(import.meta.dir, "..");
const ROOT = path.join(SDK, `.runner-cli-fixtures-${process.pid}`);
const SCRIPTS = path.join(ROOT, "scripts");
const PLUGINS = path.join(ROOT, "plugins");
fs.mkdirSync(SCRIPTS, { recursive: true });
fs.mkdirSync(PLUGINS, { recursive: true });
afterAll(() => fs.rmSync(ROOT, { recursive: true, force: true }));

// Long enough that bun's startup (~0.3 s of CPU, and it does not grow with the window) is a small
// share of what we measure.
const WINDOW_MS = 3_000;
// A spinning runner burns a whole core — one window's worth of CPU in one window. A parked one
// burns its startup and nothing after. Half the window sits far from both, so a contended CI
// runner that only lets the spinner have half a core still fails.
const MAX_CPU_MS = WINDOW_MS / 2;

test(
	"an idle runner parks instead of spinning",
	async () => {
		const proc = Bun.spawn(
			[
				process.execPath,
				"src/runner-cli.ts",
				"--scripts",
				SCRIPTS,
				"--plugins",
				PLUGINS,
			],
			{ cwd: SDK, stdout: "pipe", stderr: "pipe" },
		);
		await Bun.sleep(WINDOW_MS);
		proc.kill("SIGTERM");
		await proc.exited;
		const out = await new Response(proc.stdout).text();

		// A process that died on startup also burns no CPU — assert it really ran the runner and
		// was still alive to catch the signal, so the CPU bound below can't pass vacuously.
		expect(out).toContain("[runner] nothing to run");
		expect(out).toContain("[runner] SIGTERM");

		const cpuMs = Number(proc.resourceUsage()?.cpuTime.total ?? 0) / 1000;
		expect(cpuMs).toBeLessThan(MAX_CPU_MS);
	},
	WINDOW_MS + 15_000,
);
