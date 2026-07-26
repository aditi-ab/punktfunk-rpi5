// The library-provider wire schemas — a browser-safe module (no node imports) so plugin
// CONTRACTS can share these types with their UIs. Mirrors the host's `ProviderEntryInput`
// (crates/punktfunk-host mgmt/library.rs). Identity codecs: plain JSON shapes, so values
// pass through unencoded; the value is the shared type + authoring validation.
import { Schema } from "effect";

export const Artwork = Schema.Struct({
	portrait: Schema.optionalKey(Schema.NullOr(Schema.String)),
	hero: Schema.optionalKey(Schema.NullOr(Schema.String)),
	logo: Schema.optionalKey(Schema.NullOr(Schema.String)),
	header: Schema.optionalKey(Schema.NullOr(Schema.String)),
});
export type Artwork = typeof Artwork.Type;

export const LaunchSpec = Schema.Struct({
	kind: Schema.Literal("command"),
	value: Schema.String,
});
export type LaunchSpec = typeof LaunchSpec.Type;

export const PrepStep = Schema.Struct({
	do: Schema.String,
	undo: Schema.optionalKey(Schema.NullOr(Schema.String)),
});
export type PrepStep = typeof PrepStep.Type;

/**
 * How the host should recognize a title's process once it is running.
 *
 * Every field is optional, and omitting the whole thing is fine: the host tracks the process it
 * spawns for the entry anyway. It matters when your launch command hands off and exits — a launcher
 * client, a `flatpak run`, a front-end that starts an emulator — because then the host has nothing
 * left to watch, and the two behaviors this feeds ("end the session when the game exits" and "end the
 * game when the session ends") go quiet for that title.
 *
 * Send whatever you actually know. `install_dir` is the one worth sending if you send only one: any
 * process running from under it counts as the game.
 */
export const DetectHint = Schema.Struct({
	/** Where the title is installed (absolute path on the host). */
	install_dir: Schema.optionalKey(Schema.NullOr(Schema.String)),
	/** The game's own executable (absolute path on the host). */
	exe: Schema.optionalKey(Schema.NullOr(Schema.String)),
	/** The executable's file name (`Hades.exe`), when its location isn't fixed. Weakest signal. */
	process_name: Schema.optionalKey(Schema.NullOr(Schema.String)),
});
export type DetectHint = typeof DetectHint.Type;

export const ProviderEntry = Schema.Struct({
	external_id: Schema.String,
	title: Schema.String,
	art: Schema.optionalKey(Artwork),
	launch: Schema.optionalKey(Schema.NullOr(LaunchSpec)),
	prep: Schema.optionalKey(Schema.Array(PrepStep)),
	detect: Schema.optionalKey(DetectHint),
});
export type ProviderEntry = typeof ProviderEntry.Type;
