import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import type { PadFrame } from "@/api/gen/model/padFrame";
import {
	appendLog,
	BIT,
	BUTTON_NAMES,
	familyOf,
	LOG_MAX,
	logText,
	padEvents,
	SHAPES,
} from "./pads";

const frame = (over: Partial<PadFrame> = {}): PadFrame => ({
	pad: 0,
	ts_ms: 1_700_000_000_000,
	device: "xbox360",
	present: true,
	buttons: 0,
	left_trigger: 0,
	right_trigger: 0,
	ls_x: 0,
	ls_y: 0,
	rs_x: 0,
	rs_y: 0,
	...over,
});

// Hand-written beside core's gamepad consts; core and pf-inject test the same rows.
const vectors = JSON.parse(
	readFileSync(
		join(
			import.meta.dir,
			"../../../../crates/punktfunk-core/testdata/gamepad-button-vectors.json",
		),
		"utf8",
	),
) as { buttons: { name: string; bit: number; evdev: string }[] };

describe("the wire buttons", () => {
	test("bits match core", () => {
		expect(BIT).toEqual(
			Object.fromEntries(vectors.buttons.map((b) => [b.name, b.bit])),
		);
	});

	test("names match the virtual pad, in log order", () => {
		expect(BUTTON_NAMES).toEqual(vectors.buttons.map((b) => [b.bit, b.evdev]));
	});
});

describe("padEvents", () => {
	// The first frame is a picture, not a transition: it is what the pad was already holding.
	test("the first frame logs nothing but seeds the anchor", () => {
		const { texts, anchor } = padEvents(undefined, frame({ buttons: BIT.A }));
		expect(texts).toEqual([]);
		expect(anchor.buttons).toBe(BIT.A);
	});

	// The reporter's question, and the acceptance line: Guide must read as BTN_MODE both ways.
	test("guide reads as BTN_MODE down, then up", () => {
		const a = padEvents(undefined, frame()).anchor;
		const down = padEvents(a, frame({ buttons: BIT.GUIDE }));
		expect(down.texts).toEqual(["BTN_MODE down"]);
		const up = padEvents(down.anchor, frame({ buttons: 0 }));
		expect(up.texts).toEqual(["BTN_MODE up"]);
	});

	test("a trigger pulled to the stop reads as its value", () => {
		const a = padEvents(undefined, frame()).anchor;
		expect(padEvents(a, frame({ right_trigger: 255 })).texts).toEqual([
			"ABS_RZ 255",
		]);
	});

	// A stick sweep is one gesture. Logging every sample would fill the ring with itself.
	test("a stick logs at steps, not at every sample", () => {
		let anchor = padEvents(undefined, frame()).anchor;
		let lines = 0;
		for (let x = 0; x <= 32767; x += 256) {
			const out = padEvents(anchor, frame({ ls_x: Math.min(x, 32767) }));
			anchor = out.anchor;
			lines += out.texts.length;
		}
		expect(lines).toBeGreaterThan(0);
		expect(lines).toBeLessThan(10);
	});

	test("two flips in one frame are two lines", () => {
		const a = padEvents(undefined, frame()).anchor;
		expect(padEvents(a, frame({ buttons: BIT.A | BIT.B })).texts).toEqual([
			"BTN_SOUTH down",
			"BTN_EAST down",
		]);
	});
});

describe("the log ring", () => {
	test("never grows past its bound", () => {
		let log = appendLog([], 0, 1, [], 0);
		for (let i = 0; i < LOG_MAX * 3; i++) {
			log = appendLog(log, 0, i, [`BTN_SOUTH ${i}`], i);
		}
		expect(log).toHaveLength(LOG_MAX);
		// The tail is what survives: the newest line is still there.
		expect(log.at(-1)?.text).toBe(`BTN_SOUTH ${LOG_MAX * 3 - 1}`);
	});

	test("copy gives a paste-ready block", () => {
		const log = appendLog(
			[],
			2,
			Date.UTC(2026, 0, 1, 12, 0, 0),
			["BTN_MODE down"],
			0,
		);
		expect(logText(log)).toMatch(/^\d\d:\d\d:\d\d pad 2 BTN_MODE down$/);
	});
});

describe("shapes", () => {
	// Every kind the host can build folds onto a drawing; nothing falls through to a blank.
	test("every emulated pad kind resolves to a shape", () => {
		const kinds = [
			"xbox360",
			"xboxone",
			"xboxelite",
			"dualsense",
			"dualsenseedge",
			"dualshock4",
			"steamdeck",
			"steamcontroller",
			"steamcontroller2",
			"steamcontroller2puck",
			"switchpro",
			"8bitdoultimate2",
			"8bitdopro2",
			"8bitdopro3",
			"horipadsteam",
			"joyconpair",
			"switch2pro",
			"switch2gamecube",
			"auto",
		];
		for (const k of kinds) expect(SHAPES[familyOf(k)]).toBeDefined();
		expect(familyOf("dualshock4")).toBe("playstation");
		expect(familyOf("steamdeck")).toBe("steam");
		expect(familyOf("switchpro")).toBe("switch");
		expect(familyOf("xboxelite")).toBe("xbox");
		expect(familyOf("8bitdopro2")).toBe("switch");
		expect(familyOf("joyconpair")).toBe("switch");
		expect(familyOf("switch2pro")).toBe("switch");
		expect(familyOf("8bitdoultimate2")).toBe("xbox");
	});

	test("each shape places two sticks and a full face cluster", () => {
		for (const shape of Object.values(SHAPES)) {
			expect(shape.sticks).toHaveLength(2);
			for (const bit of [BIT.A, BIT.B, BIT.X, BIT.Y, BIT.GUIDE]) {
				expect(shape.buttons.some((b) => b.bit === bit)).toBe(true);
			}
		}
	});
});
