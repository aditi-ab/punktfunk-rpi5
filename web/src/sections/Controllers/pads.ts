// The pad vocabulary the Controllers page draws and logs: wire bits, their evdev names,
// and one geometry table per controller family.
//
// Names come from the host's own `BUTTON_MAP` (pf-inject `linux/gamepad.rs`), so a line here
// reads the way the same press reads in an evdev dump. A wire bit with no evdev counterpart
// (the D-pad, which the uinput pad emits as a hat) keeps its wire name. Both tables are
// pinned by `crates/punktfunk-core/testdata/gamepad-button-vectors.json`.

import type { PadFrame } from "@/api/gen/model/padFrame";

/** GameStream/XInput `buttonFlags` — `punktfunk_core::input::gamepad`. */
export const BIT = {
	DPAD_UP: 0x0001,
	DPAD_DOWN: 0x0002,
	DPAD_LEFT: 0x0004,
	DPAD_RIGHT: 0x0008,
	START: 0x0010,
	BACK: 0x0020,
	LS_CLICK: 0x0040,
	RS_CLICK: 0x0080,
	LB: 0x0100,
	RB: 0x0200,
	GUIDE: 0x0400,
	A: 0x1000,
	B: 0x2000,
	X: 0x4000,
	Y: 0x8000,
	PADDLE1: 0x0001_0000,
	PADDLE2: 0x0002_0000,
	PADDLE3: 0x0004_0000,
	PADDLE4: 0x0008_0000,
	TOUCHPAD: 0x0010_0000,
	MISC1: 0x0020_0000,
} as const;

/** Wire bit → the name the host's virtual pad reports it under. Log order is this order. */
export const BUTTON_NAMES: readonly (readonly [number, string])[] = [
	[BIT.A, "BTN_SOUTH"],
	[BIT.B, "BTN_EAST"],
	[BIT.X, "BTN_NORTH"],
	[BIT.Y, "BTN_WEST"],
	[BIT.LB, "BTN_TL"],
	[BIT.RB, "BTN_TR"],
	[BIT.BACK, "BTN_SELECT"],
	[BIT.START, "BTN_START"],
	[BIT.GUIDE, "BTN_MODE"],
	[BIT.LS_CLICK, "BTN_THUMBL"],
	[BIT.RS_CLICK, "BTN_THUMBR"],
	[BIT.DPAD_UP, "BTN_DPAD_UP"],
	[BIT.DPAD_DOWN, "BTN_DPAD_DOWN"],
	[BIT.DPAD_LEFT, "BTN_DPAD_LEFT"],
	[BIT.DPAD_RIGHT, "BTN_DPAD_RIGHT"],
	[BIT.PADDLE1, "BTN_TRIGGER_HAPPY5"],
	[BIT.PADDLE2, "BTN_TRIGGER_HAPPY7"],
	[BIT.PADDLE3, "BTN_TRIGGER_HAPPY6"],
	[BIT.PADDLE4, "BTN_TRIGGER_HAPPY8"],
	[BIT.TOUCHPAD, "BTN_TOUCHPAD"],
	[BIT.MISC1, "BTN_MISC1"],
];

/** The six analogue axes, named as the virtual pad reports them. */
const AXES = [
	["ls_x", "ABS_X"],
	["ls_y", "ABS_Y"],
	["rs_x", "ABS_RX"],
	["rs_y", "ABS_RY"],
	["left_trigger", "ABS_Z"],
	["right_trigger", "ABS_RZ"],
] as const;

/**
 * How far an axis moves before it earns a log line: 1/8 of its travel.
 *
 * A stick sweep is one gesture, not two hundred events. Ends (0 and full deflection) always
 * log, so "I pulled RT all the way" still reads `ABS_RZ 255`.
 */
const TRIGGER_STEP = 32;
const STICK_STEP = 8192;

/** How many log lines the page keeps. Bounded on purpose — this is a live tail, not a record. */
export const LOG_MAX = 200;

/** The last values a line was emitted for. Not the last frame: that would log every wobble. */
export interface PadAnchor {
	buttons: number;
	ls_x: number;
	ls_y: number;
	rs_x: number;
	rs_y: number;
	left_trigger: number;
	right_trigger: number;
}

export interface PadLogLine {
	/** Strictly increasing, so React keys stay stable as the ring rolls. */
	seq: number;
	ts_ms: number;
	pad: number;
	text: string;
}

export const anchorOf = (f: PadFrame): PadAnchor => ({
	buttons: f.buttons,
	ls_x: f.ls_x,
	ls_y: f.ls_y,
	rs_x: f.rs_x,
	rs_y: f.rs_y,
	left_trigger: f.left_trigger,
	right_trigger: f.right_trigger,
});

/**
 * What changed between the last logged state and this frame, as log text.
 *
 * Every button flip is a line. An axis needs to have moved a step, or to have reached an end,
 * so a stick sweep reads as a handful of lines instead of filling the ring by itself. The
 * returned anchor advances only for what was logged.
 */
export function padEvents(
	prev: PadAnchor | undefined,
	next: PadFrame,
): { texts: string[]; anchor: PadAnchor } {
	const anchor: PadAnchor = prev ? { ...prev } : anchorOf(next);
	const texts: string[] = [];
	if (!prev) return { texts, anchor };

	for (const [bit, name] of BUTTON_NAMES) {
		const was = (prev.buttons & bit) !== 0;
		const now = (next.buttons & bit) !== 0;
		if (was !== now) texts.push(`${name} ${now ? "down" : "up"}`);
	}
	anchor.buttons = next.buttons;

	for (const [field, name] of AXES) {
		const value = next[field];
		const step = field.endsWith("trigger") ? TRIGGER_STEP : STICK_STEP;
		const end =
			value === 0 || value === 255 || value === 32767 || value === -32768;
		if (
			Math.abs(value - prev[field]) >= step ||
			(end && value !== prev[field])
		) {
			texts.push(`${name} ${value}`);
			anchor[field] = value;
		}
	}
	return { texts, anchor };
}

/** Append `texts` to a bounded ring. The oldest lines fall off; nothing grows without limit. */
export function appendLog(
	log: readonly PadLogLine[],
	pad: number,
	ts_ms: number,
	texts: readonly string[],
	nextSeq: number,
): PadLogLine[] {
	if (texts.length === 0) return log as PadLogLine[];
	const added = texts.map((text, i) => ({
		seq: nextSeq + i,
		ts_ms,
		pad,
		text,
	}));
	return [...log, ...added].slice(-LOG_MAX);
}

/** `23:46:59 pad 0 BTN_MODE down` — the block Copy puts on the clipboard. */
export const logText = (log: readonly PadLogLine[]): string =>
	log
		.map(
			(l) =>
				`${new Date(l.ts_ms).toTimeString().slice(0, 8)} pad ${l.pad} ${l.text}`,
		)
		.join("\n");

// ---------------------------------------------------------------------------
// Geometry. One table per family; `PadDiagram` is the only thing that reads it.
// ---------------------------------------------------------------------------

export type PadFamily = "xbox" | "playstation" | "steam" | "switch";

/** Every kind the host emulates, folded onto the four shapes it takes. */
export function familyOf(device: string): PadFamily {
	if (device.startsWith("dualsense") || device.startsWith("dualshock"))
		return "playstation";
	if (device.startsWith("steam")) return "steam";
	// The 8BitDo Pro pads carry Nintendo labels.
	if (
		device === "switchpro" ||
		device === "joyconpair" ||
		device.startsWith("switch2") ||
		device.startsWith("8bitdopro")
	)
		return "switch";
	return "xbox";
}

interface Placed {
	bit: number;
	cx: number;
	cy: number;
	/** Short glyph inside the circle; empty for a button too small to letter. */
	label: string;
	r?: number;
}

interface Well {
	/** The stick's click bit — what lights its ring. */
	bit: number;
	cx: number;
	cy: number;
}

export interface PadShape {
	/** Body outline in the shared 260×160 viewBox. */
	body: string;
	/** Left then right; every pad the host emulates has both. */
	sticks: [Well, Well];
	dpad: { cx: number; cy: number };
	/** Face cluster plus the system row (Back/Start/Guide/Misc). */
	buttons: Placed[];
	/** Flat surfaces: a DualSense touchpad, the Deck's trackpads. */
	plates: { x: number; y: number; w: number; h: number; bit?: number }[];
}

/** A face diamond around `(cx, cy)`, with each position's bit and glyph. */
const diamond = (
	cx: number,
	cy: number,
	[bottom, right, top, left]: [Placed, Placed, Placed, Placed],
): Placed[] => [
	{ ...bottom, cx, cy: cy + 15 },
	{ ...right, cx: cx + 15, cy },
	{ ...top, cx, cy: cy - 15 },
	{ ...left, cx: cx - 15, cy },
];

const p = (bit: number, label: string): Placed => ({
	bit,
	cx: 0,
	cy: 0,
	label,
});

/** Grips-down body, the shape every family shares apart from its corners. */
const BODY_WINGED =
	"M42 34 h176 a26 26 0 0 1 26 26 v18 c0 30 -14 56 -32 56 c-14 0 -20 -14 -30 -22 h-104 c-10 8 -16 22 -30 22 c-18 0 -32 -26 -32 -56 v-18 a26 26 0 0 1 26 -26 z";
const BODY_SLAB =
	"M34 34 h192 a22 22 0 0 1 22 22 v58 a22 22 0 0 1 -22 22 h-192 a22 22 0 0 1 -22 -22 v-58 a22 22 0 0 1 22 -22 z";

export const SHAPES: Record<PadFamily, PadShape> = {
	xbox: {
		body: BODY_WINGED,
		sticks: [
			{ bit: BIT.LS_CLICK, cx: 72, cy: 62 },
			{ bit: BIT.RS_CLICK, cx: 158, cy: 98 },
		],
		dpad: { cx: 102, cy: 98 },
		buttons: [
			...diamond(196, 62, [
				p(BIT.A, "A"),
				p(BIT.B, "B"),
				p(BIT.Y, "Y"),
				p(BIT.X, "X"),
			]),
			{ bit: BIT.BACK, cx: 112, cy: 62, label: "", r: 5 },
			{ bit: BIT.START, cx: 148, cy: 62, label: "", r: 5 },
			{ bit: BIT.GUIDE, cx: 130, cy: 48, label: "G", r: 8 },
		],
		plates: [],
	},
	playstation: {
		body: BODY_WINGED,
		sticks: [
			{ bit: BIT.LS_CLICK, cx: 100, cy: 102 },
			{ bit: BIT.RS_CLICK, cx: 160, cy: 102 },
		],
		dpad: { cx: 64, cy: 62 },
		buttons: [
			...diamond(196, 62, [
				p(BIT.A, "✕"),
				p(BIT.B, "○"),
				p(BIT.Y, "△"),
				p(BIT.X, "□"),
			]),
			{ bit: BIT.BACK, cx: 92, cy: 44, label: "", r: 5 },
			{ bit: BIT.START, cx: 168, cy: 44, label: "", r: 5 },
			{ bit: BIT.GUIDE, cx: 130, cy: 112, label: "P", r: 8 },
			{ bit: BIT.MISC1, cx: 130, cy: 44, label: "", r: 4 },
		],
		plates: [{ x: 104, y: 52, w: 52, h: 32, bit: BIT.TOUCHPAD }],
	},
	steam: {
		body: BODY_SLAB,
		sticks: [
			{ bit: BIT.LS_CLICK, cx: 44, cy: 58 },
			{ bit: BIT.RS_CLICK, cx: 216, cy: 58 },
		],
		dpad: { cx: 44, cy: 104 },
		buttons: [
			...diamond(216, 104, [
				p(BIT.A, "A"),
				p(BIT.B, "B"),
				p(BIT.Y, "Y"),
				p(BIT.X, "X"),
			]),
			{ bit: BIT.BACK, cx: 106, cy: 46, label: "", r: 5 },
			{ bit: BIT.START, cx: 154, cy: 46, label: "", r: 5 },
			{ bit: BIT.GUIDE, cx: 130, cy: 46, label: "S", r: 8 },
			{ bit: BIT.MISC1, cx: 130, cy: 122, label: "", r: 5 },
		],
		plates: [
			{ x: 78, y: 62, w: 42, h: 42 },
			{ x: 140, y: 62, w: 42, h: 42 },
		],
	},
	switch: {
		body: BODY_WINGED,
		sticks: [
			{ bit: BIT.LS_CLICK, cx: 72, cy: 62 },
			{ bit: BIT.RS_CLICK, cx: 158, cy: 98 },
		],
		dpad: { cx: 102, cy: 98 },
		// Nintendo swaps the physical positions: A is right, B is bottom, X top, Y left.
		buttons: [
			...diamond(196, 62, [
				p(BIT.A, "B"),
				p(BIT.B, "A"),
				p(BIT.Y, "X"),
				p(BIT.X, "Y"),
			]),
			{ bit: BIT.BACK, cx: 108, cy: 56, label: "−", r: 6 },
			{ bit: BIT.START, cx: 152, cy: 56, label: "+", r: 6 },
			{ bit: BIT.GUIDE, cx: 148, cy: 78, label: "", r: 5 },
			{ bit: BIT.MISC1, cx: 112, cy: 78, label: "", r: 5 },
		],
		plates: [],
	},
};
