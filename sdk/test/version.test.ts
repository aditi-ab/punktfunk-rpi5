// The one thing that keeps `SDK_VERSION` honest. The runner compares it against the SDK actually
// installed in the plugins tree and reinstalls on a mismatch, so a stale constant would either
// reinstall forever (constant behind) or never deliver a fix (constant ahead of a release).
import { readFileSync } from "node:fs";
import { describe, expect, test } from "bun:test";
import { SDK_VERSION } from "../src/version.js";

describe("SDK_VERSION", () => {
	test("matches package.json — bump both or neither", () => {
		// Read rather than import: `tsconfig.build.json` pins `rootDir: "src"`, so a JSON import of
		// the manifest would not compile for the npm build even though bun would run it fine.
		const pkg = JSON.parse(
			readFileSync(new URL("../package.json", import.meta.url), "utf8"),
		) as { version: string };
		expect(SDK_VERSION).toBe(pkg.version);
	});

	test("is a plain semver triple", () => {
		// The runner compares it to an installed version string, so anything with a range operator
		// (`^0.1.3`) would never compare equal and would reinstall on every start.
		expect(SDK_VERSION).toMatch(/^\d+\.\d+\.\d+$/);
	});
});
