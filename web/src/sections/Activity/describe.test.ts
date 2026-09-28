import { expect, test } from "bun:test";
import { describe as describeEntry } from "./index";

test("a game row names the client and the game", () => {
	const line = describeEntry({
		seq: 1,
		ts_ms: 0,
		kind: "game.running",
		data: {
			kind: "game.running",
			game: { client: "Deck", title: "Celeste", plane: "native" },
		},
	});
	expect(line).toBe("Deck · Celeste");
});

test("an access row names the device", () => {
	const line = describeEntry({
		seq: 2,
		ts_ms: 0,
		kind: "access.expired",
		data: { kind: "access.expired", device: { name: "Guest phone" } },
	});
	expect(line).toBe("Guest phone");
});
