import type { AudioSessions } from "@/api/gen/model/audioSessions";
import type { CustomEntry } from "@/api/gen/model/customEntry";
import type { CustomInput } from "@/api/gen/model/customInput";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import type { LaunchSpec } from "@/api/gen/model/launchSpec";
import type { PrepCmd } from "@/api/gen/model/prepCmd";
import { carriesCommandExecution } from "@/lib/command-execution";

/** The entry page's one draft. Numbers and lists stay the raw text typed; `toInput` parses them. */
export interface FormState {
	title: string;
	portrait: string;
	hero: string;
	header: string;
	logo: string;
	/** Brand token (`GameEntry.icon`); empty = none. */
	icon: string;
	command: string;
	/** The stored launch when it is not a command (`steam_appid`, `exec`): sent back untouched. */
	launch: LaunchSpec | null;
	/** `true` = this entry opens a launcher rather than a game; it moves to the Launchers rail. */
	isLauncher: boolean;
	// The host's own row: process hints and prep. `hintsLoaded` says it answered, so a blank
	// hint is sent as cleared rather than omitted (= kept).
	exe: string;
	installDir: string;
	processName: string;
	prep?: PrepCmd[];
	hintsLoaded: boolean;
	/** Which sessions hear the title (`audio.sessions`). `all` is the host's "no policy". */
	audioSessions: AudioSessions;
	platform: string;
	description: string;
	developer: string;
	publisher: string;
	releaseYear: string;
	genres: string;
	tags: string;
	region: string;
	players: string;
}

export const emptyForm: FormState = {
	title: "",
	portrait: "",
	hero: "",
	header: "",
	logo: "",
	icon: "",
	command: "",
	launch: null,
	isLauncher: false,
	exe: "",
	installDir: "",
	processName: "",
	hintsLoaded: false,
	audioSessions: "all",
	platform: "",
	description: "",
	developer: "",
	publisher: "",
	releaseYear: "",
	genres: "",
	tags: "",
	region: "",
	players: "",
};

/** The catalog entry as a draft. The hints stay blank until `withStored` folds in the row. */
export function formFrom(entry: GameEntry): FormState {
	return {
		...emptyForm,
		title: entry.title,
		portrait: entry.art.portrait ?? "",
		hero: entry.art.hero ?? "",
		header: entry.art.header ?? "",
		logo: entry.art.logo ?? "",
		icon: entry.icon ?? "",
		command: entry.launch?.kind === "command" ? entry.launch.value : "",
		launch: entry.launch ?? null,
		isLauncher: entry.role === "launcher",
		platform: entry.platform ?? "",
		description: entry.description ?? "",
		developer: entry.developer ?? "",
		publisher: entry.publisher ?? "",
		releaseYear: entry.release_year?.toString() ?? "",
		genres: entry.genres?.join(", ") ?? "",
		tags: entry.tags?.join(", ") ?? "",
		region: entry.region ?? "",
		players: entry.players?.toString() ?? "",
	};
}

/** Fold the host's own row in: hints, prep, audio. No row leaves `hintsLoaded` false, so
 * `toInput` omits them and the host keeps what it has. */
export function withStored(
	f: FormState,
	stored: CustomEntry | undefined,
): FormState {
	if (!stored) return f;
	return {
		...f,
		exe: stored.detect?.exe ?? "",
		installDir: stored.detect?.install_dir ?? "",
		processName: stored.detect?.process_name ?? "",
		prep: stored.prep ?? [],
		audioSessions: stored.audio?.sessions ?? "all",
		hintsLoaded: true,
	};
}

/** A custom entry's draft, from its stored row only. The catalog's art is rewritten to the
 * art proxy, and a proxy path saved back as art points at nothing. */
export function formFromStored(row: CustomEntry): FormState {
	return withStored(
		formFrom({ ...row, art: row.art ?? {}, store: row.store ?? "custom" }),
		row,
	);
}

/** The draft as the API body. `update_custom` replaces title, art, launch, role, icon and meta
 * wholesale, so every one of them is sent every time. */
export function toInput(f: FormState): CustomInput {
	const trim = (s: string) => {
		const t = s.trim();
		return t ? t : undefined;
	};
	const list = (s: string) => {
		const items = s
			.split(",")
			.map((x) => x.trim())
			.filter(Boolean);
		return items.length ? items : undefined;
	};
	const int = (s: string) => {
		const n = Number.parseInt(s.trim(), 10);
		return Number.isFinite(n) ? n : undefined;
	};
	const command = f.command.trim();
	// A typed command wins; a cleared one clears; any other stored launch goes back as it came.
	const launch: LaunchSpec | null = command
		? { kind: "command", value: command }
		: f.launch?.kind === "command"
			? null
			: f.launch;
	return {
		title: f.title.trim(),
		art: {
			portrait: trim(f.portrait),
			hero: trim(f.hero),
			header: trim(f.header),
			logo: trim(f.logo),
		},
		icon: trim(f.icon) ?? null,
		launch,
		...(f.isLauncher ? { role: "launcher" as const } : {}),
		// Omitted means "keep" on the host: sent once the row was read or a hint was typed.
		...(f.hintsLoaded ||
		trim(f.exe) ||
		trim(f.installDir) ||
		trim(f.processName)
			? {
					detect: {
						exe: trim(f.exe),
						install_dir: trim(f.installDir),
						process_name: trim(f.processName),
					},
				}
			: {}),
		...(f.prep ? { prep: f.prep } : {}),
		...(f.hintsLoaded || f.audioSessions !== "all"
			? { audio: { sessions: f.audioSessions } }
			: {}),
		platform: trim(f.platform),
		description: trim(f.description),
		developer: trim(f.developer),
		publisher: trim(f.publisher),
		release_year: int(f.releaseYear),
		genres: list(f.genres),
		tags: list(f.tags),
		region: trim(f.region),
		players: int(f.players),
	};
}

/** The body the BFF expects: the console password rides along only when the gate applies. */
export function withPassword(
	input: CustomInput,
	password: string,
): CustomInput & { password?: string } {
	return carriesCommandExecution(input) ? { ...input, password } : input;
}
