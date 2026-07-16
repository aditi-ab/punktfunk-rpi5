import { defineConfig } from "orval";

// Generates the SDK's Effect Schemas from the host's checked-in OpenAPI document — the same
// single source of truth the web console's react-query client is generated from (RFC §7: the
// chain is Rust structs (utoipa) → OpenAPI → Effect Schemas → inferred TS types). Regenerate
// after any management-API change: `bun run gen` (CI drift-tests the output).
export default defineConfig({
	punktfunk: {
		input: {
			target: "../api/openapi.json",
		},
		output: {
			mode: "single",
			target: "./src/gen/schemas.ts",
			client: "effect",
			clean: true,
		},
	},
});
