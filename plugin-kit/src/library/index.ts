// `@punktfunk/plugin-kit/library` — the shared framework for library-scanner plugins.
//
// A first-party scanner is its parsers plus a scan function; everything else (store claim, sync
// engine wiring, launcher entries, `__config`, nav category, CLI verbs) comes from
// `defineLibraryPlugin`. See design/library-scanner-plugins.md D10.
export {
	defineLibraryPlugin,
	type LibraryPlugin,
	type LibraryPluginDef,
	type ScanReport,
} from "./define.js";
export * from "./parsers/index.js";
