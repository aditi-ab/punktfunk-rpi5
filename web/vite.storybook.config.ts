import { defineConfig } from "vite";
import viteReact from "@vitejs/plugin-react";
import viteTsConfigPaths from "vite-tsconfig-paths";
import tailwindcss from "@tailwindcss/vite";
import { paraglideVitePlugin } from "@inlang/paraglide-js";

// Storybook builds the components in isolation — WITHOUT the TanStack Start /
// Nitro plugins from vite.config.ts. Keeps the `@/*` alias, Tailwind v4, the
// React transform, and Paraglide.
export default defineConfig({
	plugins: [
		viteTsConfigPaths({ projects: ["./tsconfig.json"] }),
		tailwindcss(),
		viteReact(),
		paraglideVitePlugin({
			project: "./project.inlang",
			outdir: "./src/paraglide",
			strategy: ["localStorage", "preferredLanguage", "baseLocale"],
		}),
	],
});
