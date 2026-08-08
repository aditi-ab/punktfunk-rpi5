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

/**
 * How the host should launch a title. **The host owns this vocabulary** — it validates the value
 * per kind and builds the actual URI / command line itself, so a plugin only ever supplies a
 * validated value, never a command. That is the security invariant behind the whole provider lane:
 * a client sends an entry id, and the host resolves what to run. (`plugin`, below, is the one kind
 * whose command the plugin composes — but it is still never *stored*: the host asks the live plugin
 * at launch time, so an entry on its own executes nothing.)
 *
 * `kind` is a plain string rather than a union so the kit never has to ship a release to keep up
 * with a host that grew a new kind. The kinds the host understands today:
 *
 * | kind | value | platforms |
 * |---|---|---|
 * | `command` | a shell command (operator-trust tier) | both |
 * | `steam_appid` | digits — an appid, or a 64-bit non-Steam-shortcut game id | both |
 * | `steam_ui` | `bigpicture` \| `desktop` — opens the Steam client itself | both |
 * | `launcher_ui` | a store id (`heroic`, `lutris`) — opens that launcher's own UI | linux |
 * | `lutris_id` | digits — a pga.db game id | linux |
 * | `heroic` | `<runner>:<appName>`, runner ∈ legendary/gog/nile | linux |
 * | `epic` | `<namespace>:<catalogItemId>:<appName>` or a bare appName | windows |
 * | `gog` | `exe \t args \t workdir` | windows |
 * | `aumid` | `<PFN>!<AppId>` | windows |
 * | `plugin` | an opaque key in THIS plugin's namespace — see below | both |
 *
 * `plugin` is the escape hatch for a tile the host cannot name on its own (a ROM through whichever
 * emulator the operator configured). The value is meaningless to the host: it hands the key back to
 * the plugin that published the entry, on its own loopback UI port, and runs the command line that
 * comes back. Serve it with `serveUi({launch})`; a plugin that publishes this kind without serving
 * `/__launch` grows unlaunchable tiles.
 *
 * An unknown kind is accepted on the wire and simply yields no launch recipe on that host, so a
 * plugin targeting a newer host degrades to an unlaunchable tile rather than a failed reconcile.
 */
export const LaunchSpec = Schema.Struct({
	kind: Schema.String,
	value: Schema.String,
});
export type LaunchSpec = typeof LaunchSpec.Type;

/**
 * Whether an entry is an ordinary title or the launcher application itself (Steam Big Picture,
 * Heroic, Playnite fullscreen). Launcher entries launch, lease and list exactly like games; a
 * console or client that knows the field groups them into their own rail, and one that doesn't
 * renders them as plain tiles.
 */
export const GameRole = Schema.Literals(["game", "launcher"]);
export type GameRole = typeof GameRole.Type;

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
	/**
	 * The Steam appid, for a title Steam itself installed. On Linux this is the **sharpest** signal
	 * there is: Steam wraps every launch — native or Proton — in `reaper SteamLaunch AppId=<appid>`,
	 * whose lifetime is exactly the game's. Send it if you have it.
	 */
	steam_appid: Schema.optionalKey(Schema.NullOr(Schema.Number)),
	/**
	 * An environment variable the launcher stamps on the game's process. Load-bearing for launchers
	 * that run games under Proton/Wine, where the process tree tells you very little (Heroic's
	 * `HEROIC_APP_NAME` is the verified case). Omit `value` to match on the key's mere presence —
	 * only safe for a launcher that runs one game at a time.
	 */
	env_marker: Schema.optionalKey(
		Schema.NullOr(
			Schema.Struct({
				/** `[A-Za-z0-9_]{1,64}` — the host rejects anything else. */
				key: Schema.String,
				/** At most 256 chars. */
				value: Schema.optionalKey(Schema.NullOr(Schema.String)),
			}),
		),
	),
});
export type DetectHint = typeof DetectHint.Type;

/** Descriptive metadata, flat on the wire beside `title` (mirrors the host's flattened
 * `GameMeta`). All fields optional; values are free-form display strings — the host does not
 * normalize platform/genre vocabularies. */
export const GameMeta = Schema.Struct({
	/** The system the title runs on — `"PS2"`, `"Xbox 360"`, `"SNES"`, … */
	platform: Schema.optionalKey(Schema.NullOr(Schema.String)),
	/** Short blurb for a details pane. */
	description: Schema.optionalKey(Schema.NullOr(Schema.String)),
	developer: Schema.optionalKey(Schema.NullOr(Schema.String)),
	publisher: Schema.optionalKey(Schema.NullOr(Schema.String)),
	/** Year of first release. */
	release_year: Schema.optionalKey(Schema.NullOr(Schema.Number)),
	/** Genre taxonomy from the metadata source (`"RPG"`, `"Platformer"`, …). */
	genres: Schema.optionalKey(Schema.Array(Schema.String)),
	/** Free-form organizational labels (`"co-op"`, `"kids"`, …). */
	tags: Schema.optionalKey(Schema.Array(Schema.String)),
	/** Release region — `"NTSC-U"`, `"PAL"`, `"NTSC-J"`. */
	region: Schema.optionalKey(Schema.NullOr(Schema.String)),
	/** Maximum simultaneous (local) players. */
	players: Schema.optionalKey(Schema.NullOr(Schema.Number)),
});
export type GameMeta = typeof GameMeta.Type;

export const ProviderEntry = Schema.Struct({
	external_id: Schema.String,
	title: Schema.String,
	art: Schema.optionalKey(Artwork),
	launch: Schema.optionalKey(Schema.NullOr(LaunchSpec)),
	prep: Schema.optionalKey(Schema.Array(PrepStep)),
	detect: Schema.optionalKey(DetectHint),
	/** `"game"` (default) or `"launcher"` — see {@link GameRole}. */
	role: Schema.optionalKey(GameRole),
	...GameMeta.fields,
});
export type ProviderEntry = typeof ProviderEntry.Type;
