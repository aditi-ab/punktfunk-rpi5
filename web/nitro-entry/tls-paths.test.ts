// The pair swap is all-or-nothing, and the fallbacks are what keep legacy and custom-cert hosts
// serving. A native cert with the legacy key would be a console nobody can handshake with, so the
// mixed cases are the ones worth pinning down.
import { describe, expect, it } from "bun:test";
import { resolveUiTlsPaths } from "./tls-paths.mjs";

const DIR = "/home/you/.config/punktfunk";
const legacy = [`${DIR}/cert.pem`, `${DIR}/key.pem`] as const;
const native = [`${DIR}/native-cert.pem`, `${DIR}/native-key.pem`] as const;
/** `exists` over a fixed set of files on disk. */
const on =
	(...files: string[]) =>
	(p: string) =>
		files.includes(p);

describe("resolveUiTlsPaths", () => {
	it("prefers the native pair when both files are there", () => {
		expect(resolveUiTlsPaths(...legacy, on(...legacy, ...native))).toEqual({
			cert: native[0],
			key: native[1],
		});
	});

	it("keeps the legacy pair on a host that never took the identity split", () => {
		expect(resolveUiTlsPaths(...legacy, on(...legacy))).toEqual({
			cert: legacy[0],
			key: legacy[1],
		});
	});

	it("never mixes halves when only one native file exists", () => {
		for (const half of native) {
			expect(resolveUiTlsPaths(...legacy, on(...legacy, half))).toEqual({
				cert: legacy[0],
				key: legacy[1],
			});
		}
	});

	it("leaves an operator's own cert alone, native pair present or not", () => {
		const own = [`${DIR}/lan-ca.pem`, `${DIR}/lan-ca.key`] as const;
		expect(resolveUiTlsPaths(...own, on(...own, ...native))).toEqual({
			cert: own[0],
			key: own[1],
		});
	});

	it("passes a half-configured pair through for the entry to refuse", () => {
		expect(resolveUiTlsPaths(legacy[0], undefined, on(...native))).toEqual({
			cert: legacy[0],
			key: undefined,
		});
		expect(resolveUiTlsPaths(undefined, undefined, on(...native))).toEqual({
			cert: undefined,
			key: undefined,
		});
	});
});
