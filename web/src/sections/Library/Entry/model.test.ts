import { describe, expect, test } from "bun:test";
import type { CustomEntry } from "@/api/gen/model/customEntry";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import {
	emptyForm,
	formFrom,
	formFromStored,
	toInput,
	withPassword,
	withStored,
} from "./model";

const entry: GameEntry = {
	id: "custom:abc",
	store: "custom",
	title: "Celeste",
	art: {
		portrait: "https://x/p.png",
		hero: null,
		header: "https://x/h.png",
		logo: "https://x/l.png",
	},
	icon: "steam",
	launch: { kind: "steam_appid", value: "504230" },
	role: "launcher",
	platform: "PC",
	description: "Climb.",
	developer: "EXOK",
	publisher: "EXOK",
	release_year: 2018,
	players: 1,
	region: "EU",
	genres: ["Platformer", "Indie"],
	tags: ["hard"],
};

const stored: CustomEntry = {
	id: "abc",
	title: "Celeste",
	art: entry.art,
	detect: { exe: "Celeste.exe", install_dir: null, process_name: null },
	prep: [{ do: "echo hi", undo: null }],
	audio: { sessions: "owner" },
};

describe("entry model", () => {
	test("an edit sends back what it did not touch", () => {
		const input = toInput(formFrom(entry));
		expect(input.icon).toBe("steam");
		expect(input.launch).toEqual({ kind: "steam_appid", value: "504230" });
		expect(input.role).toBe("launcher");
		expect(input.art).toEqual({
			portrait: "https://x/p.png",
			hero: undefined,
			header: "https://x/h.png",
			logo: "https://x/l.png",
		});
		expect(input.genres).toEqual(["Platformer", "Indie"]);
		expect(input.release_year).toBe(2018);
		expect(input.players).toBe(1);
		expect(input.region).toBe("EU");
	});

	test("a typed command replaces the stored launch, a cleared one clears it", () => {
		const f = formFrom(entry);
		expect(toInput({ ...f, command: " run.sh " }).launch).toEqual({
			kind: "command",
			value: "run.sh",
		});
		const cmd = formFrom({
			...entry,
			launch: { kind: "command", value: "run.sh" },
		});
		expect(cmd.command).toBe("run.sh");
		expect(toInput({ ...cmd, command: "" }).launch).toBeNull();
	});

	test("the stored row's hints and prep go back; without it they are omitted", () => {
		const read = toInput(withStored(formFrom(entry), stored));
		expect(read.detect?.exe).toBe("Celeste.exe");
		expect(read.prep).toEqual([{ do: "echo hi", undo: null }]);
		expect(read.audio).toEqual({ sessions: "owner" });
		const unread = toInput(withStored(formFrom(entry), undefined));
		expect("detect" in unread).toBe(false);
		expect("prep" in unread).toBe(false);
		expect("audio" in unread).toBe(false);
	});

	test("a stored row seeds the raw art, not the proxy path", () => {
		const f = formFromStored({
			...stored,
			art: { portrait: "https://cdn/p.jpg", hero: "/games/c/hero.png" },
			icon: "gog",
			launch: { kind: "exec", value: "celeste" },
		});
		expect(f.portrait).toBe("https://cdn/p.jpg");
		expect(f.hero).toBe("/games/c/hero.png");
		expect(f.hintsLoaded).toBe(true);
		expect(toInput(f).launch).toEqual({ kind: "exec", value: "celeste" });
		expect(toInput(f).icon).toBe("gog");
	});

	test("an empty icon clears it", () => {
		expect(toInput({ ...emptyForm, title: "x" }).icon).toBeNull();
	});
});

describe("password gate", () => {
	test("prep without a command still asks", () => {
		const f = withStored(formFrom({ ...entry, launch: null }), stored);
		const body = withPassword(toInput(f), "pw");
		expect(body.password).toBe("pw");
	});

	test("no gate, no password", () => {
		const body = withPassword(toInput(formFrom(entry)), "pw");
		expect("password" in body).toBe(false);
	});
});
