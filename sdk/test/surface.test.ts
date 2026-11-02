// A plugin imports the SDK by name, and the runner swaps the host's SDK under an installed plugin
// on every host upgrade. A dropped export breaks plugins already in the field, so the names each
// entry point exports are a snapshot: a change shows in the diff. `bun test -u` rewrites it.
import { expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

const SDK = path.join(import.meta.dir, "..");

/** Every name a `.d.ts` exports, `export *` followed. A type-only export reads `type Name`. */
const exported = (file: string): string[] =>
	fs
		.readFileSync(file, "utf8")
		.split("\n")
		.filter((line) => line.startsWith("export "))
		.flatMap((line) => {
			const star = /^export \* from "(\.[^"]+)\.js";/.exec(line);
			if (star) return exported(path.join(path.dirname(file), `${star[1]}.d.ts`));
			const namespace = /^export \* as (\w+) from /.exec(line);
			if (namespace) return [namespace[1]];
			const list = /^export (type )?\{([^}]*)\}/.exec(line);
			if (list)
				return list[2]
					.split(",")
					.map((item) => item.trim())
					.filter(Boolean)
					.map((item) => {
						const name = item.replace(/^type /, "").split(" as ").pop();
						return list[1] || item.startsWith("type ") ? `type ${name}` : `${name}`;
					});
			const declared =
				/^export (?:declare )?(?:abstract )?(const|function|class|enum|namespace|interface|type) (\w+)/.exec(
					line,
				);
			if (declared)
				return [/interface|type/.test(declared[1]) ? `type ${declared[2]}` : declared[2]];
			throw new Error(`${file}: an export this test can't read: ${line}`);
		});

test("each entry point exports the names in the snapshot", () => {
	const out = fs.mkdtempSync(path.join(os.tmpdir(), "sdk-surface-"));
	try {
		const tsc = Bun.spawnSync(
			[process.execPath, "x", "tsc", "-p", "tsconfig.build.json", "--emitDeclarationOnly", "--outDir", out],
			{ cwd: SDK },
		);
		expect(tsc.exitCode, tsc.stdout.toString() + tsc.stderr.toString()).toBe(0);
		const { exports: entries } = JSON.parse(
			fs.readFileSync(path.join(SDK, "package.json"), "utf8"),
		) as { exports: Record<string, { types: string }> };
		const surface = Object.fromEntries(
			Object.entries(entries).map(([entry, { types }]) => [
				entry,
				[...new Set(exported(path.join(out, path.relative("dist", types))))].sort(),
			]),
		);
		expect(surface).toMatchSnapshot();
	} finally {
		fs.rmSync(out, { recursive: true, force: true });
	}
});
