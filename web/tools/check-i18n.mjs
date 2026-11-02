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

// The UI copy budget (design/web-console-overhaul.md D2, docs/writing.md §4b). A control is
// made clear by its options, not by a paragraph under it — the measured baseline was 47 strings
// over 120 characters, three of them over 550, each sitting under a two-button toggle.
//
// Anything an operator might still want to know goes to docs-site behind a `Docs ↗` link.
const HINT = /(_help|_hint|_note|_intro)$/;
const HINT_MAX = 110;
const OTHER_MAX = 200;
// Translations get 20% more room: German runs longer than English for the same sentence, and
// squeezing it back under an English budget buys stilted German, not a clearer control. The
// base locale is where the copy is authored, so that is where the real limit bites.
const TRANSLATION_SLACK = 1.2;

// Legal and security texts, where the exact wording is the point and a Docs link is not a
// substitute for reading it. Every entry here is a debt line in the design doc §2.2.
const LONG_OK = new Set([
	"store_spec_lead", // installing from a raw package registry: no catalog, no review
	"store_install_external_note", // third-party catalog code runs with the runner's privileges
	"store_update_all_external_note", // the same, for a bulk update
]);

const limit = (key, isBase) =>
	Math.round(
		(HINT.test(key) ? HINT_MAX : OTHER_MAX) * (isBase ? 1 : TRANSLATION_SLACK),
	);

const tooLong = [];
const keys = {};
for (const locale of settings.locales) {
	const path = resolve(project, "..", pathPattern.replace("{locale}", locale));
	if (!existsSync(path))
		fail(`messages for locale "${locale}" missing at ${path}`);
	const messages = JSON.parse(readFileSync(path, "utf8"));
	keys[locale] = Object.keys(messages).filter((k) => !k.startsWith("$"));
	for (const [key, value] of Object.entries(messages)) {
		if (key.startsWith("$") || typeof value !== "string") continue;
		if (LONG_OK.has(key)) continue;
		const max = limit(key, locale === settings.baseLocale);
		if (value.length > max) {
			tooLong.push(`  ${locale}/${key}: ${value.length} chars (max ${max})`);
		}
	}
}
if (tooLong.length > 0) {
	fail(
		`${tooLong.length} message(s) over the UI copy budget — shorten them, or move the detail ` +
			`into docs-site behind a Docs link (docs/writing.md §4b):\n${tooLong.join("\n")}`,
	);
}

// Locale parity. A key only a translation has is one the base locale dropped or never had, and
// nothing renders it: that fails. A key a translation lacks is reported and passes — translating
// it is work of its own, and the count is what shows the gap growing.
const base = new Set(keys[settings.baseLocale]);
const lacking = [];
for (const locale of settings.locales) {
	if (locale === settings.baseLocale) continue;
	const orphans = keys[locale].filter((k) => !base.has(k));
	if (orphans.length > 0) {
		fail(
			`${orphans.length} key(s) exist only in "${locale}" — remove them, or add them to ` +
				`"${settings.baseLocale}":\n${orphans.map((k) => `  ${k}`).join("\n")}`,
		);
	}
	const missing = base.size - keys[locale].length;
	if (missing > 0) lacking.push(`${locale} lacks ${missing} of ${base.size}`);
}

console.log(
	`✔ i18n check: ${emitted} compiled messages for ${settings.locales.join(", ")}, all within the copy budget`,
);
if (lacking.length > 0)
	console.log(`  untranslated: ${lacking.join(", ")} keys`);
