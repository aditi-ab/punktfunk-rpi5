// What a card says about where a profile plays, per seat state.
import { expect, test } from "bun:test";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { SeatPublic } from "@/api/gen/model/seatPublic";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { playsLine } from "./view";

overwriteGetLocale(() => baseLocale);

const card = (seat?: SeatPublic): ProfileAdmin => ({
	id: "5a7c9e1b3d5f",
	display_name: "Mia",
	owner: false,
	home: "desktop",
	seat,
	last_used_unix: 0,
	default: false,
});

const line = (seat: Omit<SeatPublic, "kind">) =>
	playsLine(card({ ...seat, kind: "desktop" }), "Enrico");

test("a seat names its own desktop", () => {
	expect(line({ state: "ready", port: 1 })).toBe("Own desktop");
	expect(line({ state: "stopped", port: 1 })).toBe("Own desktop");
});

test("starting shows the host's progress, else a plain wait line", () => {
	expect(line({ state: "starting", port: 1, detail: "Signing in" })).toBe(
		"Signing in",
	);
	expect(line({ state: "starting", port: 1 })).toBe("Getting ready…");
});

test("occupied names the device, unavailable the host's reason", () => {
	expect(line({ state: "occupied", port: 1, occupant: "Leon's Deck" })).toBe(
		"Playing on Leon's Deck",
	);
	expect(line({ state: "unavailable", port: 1, detail: "No licence" })).toBe(
		"No licence",
	);
});

test("a profile without a seat shares the owner's desktop", () => {
	expect(playsLine(card(), "Enrico")).toBe("Shares Enrico's desktop");
});

test("a door gives a desktop sharer the owner's seat and still calls it sharing", () => {
	const shared = card({ state: "stopped", port: 9778, kind: "shared" });
	expect(playsLine(shared, "Enrico")).toBe("Shares Enrico's desktop");
	const own = card({ state: "ready", port: 9779, kind: "desktop" });
	expect(playsLine(own, "Enrico")).toBe("Own desktop");
});
