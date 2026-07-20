// Guard: assert paraglide actually compiled our translations.
//
// The inlang SDK treats a failed plugin import as a WARNING, not an error: if the
// message-format plugin can't be loaded, `paraglide-js compile` still prints
// "Successfully compiled" and exits 0 — having emitted an empty message set. vite
// then happily bundles that, and the console only dies at SSR time, deep inside
// renderToReadableStream, because every `m.foo()` is undefined. That shipped once
// (a Nix build, where the plugin was fetched from a CDN in a network-off sandbox),
// so the build now fails loudly instead.
//
//   node tools/check-i18n.mjs          # run from web/, after `paraglide-js compile`
//
// Env knobs: PARAGLIDE_OUTDIR (default ./src/paraglide), INLANG_PROJECT (default
// ./project.inlang).

import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

const outdir = resolve(process.env.PARAGLIDE_OUTDIR ?? "./src/paraglide");
const project = resolve(process.env.INLANG_PROJECT ?? "./project.inlang");

const fail = (msg) => {
	console.error(`ERROR: i18n check failed — ${msg}`);
	process.exit(1);
};

const settingsPath = join(project, "settings.json");
if (!existsSync(settingsPath)) fail(`no inlang settings at ${settingsPath}`);
const settings = JSON.parse(readFileSync(settingsPath, "utf8"));

// Every plugin must resolve offline. A remote module makes the build depend on a CDN
// at compile time, which silently yields zero messages in any sandbox without network
// (Nix, air-gapped CI) — see the header.
for (const mod of settings.modules ?? []) {
	if (/^https?:/i.test(mod)) {
		fail(
			`inlang module "${mod}" is a remote URL — vendor it as a devDependency and reference it by path, ` +
				`else offline builds compile zero messages`,
		);
	}
	const modPath = resolve(project, "..", mod);
	if (!existsSync(modPath))
		fail(
			`inlang module "${mod}" does not exist at ${modPath} (run \`bun install\`?)`,
		);
}

// The base-locale source file is the source of truth for how many messages we expect.
const pathPattern = settings["plugin.inlang.messageFormat"]?.pathPattern;
if (!pathPattern)
	fail("settings.json has no plugin.inlang.messageFormat.pathPattern");
const basePath = resolve(
	project,
	"..",
	pathPattern.replace("{locale}", settings.baseLocale),
);
if (!existsSync(basePath)) fail(`base-locale messages missing at ${basePath}`);
const expected = Object.keys(JSON.parse(readFileSync(basePath, "utf8"))).filter(
	(k) => !k.startsWith("$"),
).length;
if (expected === 0) fail(`base-locale messages at ${basePath} are empty`);

const messagesDir = join(outdir, "messages");
if (!existsSync(messagesDir))
	fail(`paraglide emitted no messages dir at ${messagesDir}`);
// paraglide writes one module per message plus an `_index.js` barrel.
const emitted = readdirSync(messagesDir).filter(
	(f) => f.endsWith(".js") && f !== "_index.js",
).length;

if (emitted < expected) {
	fail(
		`paraglide emitted ${emitted} messages but ${basePath} defines ${expected}. ` +
			`The message-format plugin most likely failed to import — check the compile output for PluginImportError.`,
	);
}

console.log(
	`✔ i18n check: ${emitted} compiled messages for ${settings.locales.join(", ")}`,
);
