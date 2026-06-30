import { fileURLToPath } from "node:url";
import { paraglideVitePlugin } from "@inlang/paraglide-js";
import tailwindcss from "@tailwindcss/vite";
import { nitroV2Plugin } from "@tanstack/nitro-v2-vite-plugin";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import viteReact from "@vitejs/plugin-react";
import { defineConfig } from "vite";
import viteTsConfigPaths from "vite-tsconfig-paths";

// Absolute path to our Nitro server source (middleware + routes). Passed as a scanDir
// because the TanStack Nitro plugin doesn't auto-scan a server/ dir.
const serverDir = fileURLToPath(new URL("./server", import.meta.url));

// The management API the console drives. The browser always talks same-origin (/api/...):
// in `vite dev` the dev server proxies it (below); in the built Bun/Nitro server a Nitro
// route-rule proxies it (below). Override the upstream with PUNKTFUNK_MGMT_URL.
const MGMT_URL = process.env.PUNKTFUNK_MGMT_URL ?? "https://127.0.0.1:47990";

export default defineConfig({
	server: {
		proxy: {
			// `secure: false`: the host serves its own self-signed identity cert on loopback.
			"/api": { target: MGMT_URL, changeOrigin: true, secure: false },
		},
	},
	plugins: [
		viteTsConfigPaths({ projects: ["./tsconfig.json"] }),
		tailwindcss(),
		paraglideVitePlugin({
			project: "./project.inlang",
			outdir: "./src/paraglide",
			strategy: ["localStorage", "preferredLanguage", "baseLocale"],
		}),
		// Full SSR on the TanStack Start runtime (the management console's data queries run
		// client-side after hydration — React Query doesn't fetch during SSR — so the server
		// renders a data-free shell that hydrates in the browser).
		tanstackStart(),
		// Nitro v2 is the deployment target: the `bun` preset bundles a Bun-runnable server to
		// .output/ (`bun run .output/server/index.mjs`). Auth + the /api proxy live in the
		// scanned `server/` dir (middleware/auth.ts gates every request; routes/api/[...].ts
		// proxies to the management host injecting the bearer token server-side) — NOT a static
		// routeRule, so the proxy runs behind the login gate and reads env at runtime.
		nitroV2Plugin({
			// bun + a CUSTOM entry: Nitro's `bun` preset bundles the handler, and `entry` swaps the
			// stock self-listening entry for ours (`nitro-entry/bun-https.mjs`), which calls
			// `Bun.serve({ tls })` so the console is served over HTTPS (HTTP/1.1 over TLS) with the
			// host's own identity cert. (No HTTP/2 — Bun.serve has no h2 server — and no HTTP/3, which a
			// browser won't speak against this self-signed, no-SAN host cert.) Bun is the runtime
			// everywhere now — the Windows installer already bundles it, and the punktfunk-web .deb
			// vendors it (it can't be `node`: `Bun.serve` is a bun API). (dev `vite dev` is unaffected.)
			preset: "bun",
			entry: fileURLToPath(
				new URL("./nitro-entry/bun-https.mjs", import.meta.url),
			),
			// BUNDLE every dependency into the server output (no externalized node_modules). Three wins:
			// (1) the .output tree drops from ~47k files / 730 MB (the whole untree-shaken @unom/ui dep
			// tree — payload, lexical, date-fns…) to a handful of tree-shaken chunks; (2) the output is a
			// self-contained ~75-file `.output` the bundled `bun` runs directly (the Windows installer
			// ships bun + that `.output`, not node + a node_modules forest); (3) it removes the
			// bare external imports (`srvx`, `seroval`…) bun couldn't resolve at runtime — the reason we
			// used to need node. node still runs the same self-contained output for the Linux .deb.
			noExternals: true,
			compatibilityDate: "2026-06-10",
			// Scan server/{middleware,routes} for the auth gate + the /api proxy.
			scanDirs: [serverDir],
		}),
		// Must come AFTER tanstackStart — provides the React JSX transform + Refresh runtime
		// that Start's dev mode requires (omitting it leaves the client JS unable to load).
		viteReact(),
	],
});
