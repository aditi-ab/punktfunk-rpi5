import { describe, expect, test } from "bun:test";
import { fmtClockDuration } from "./format";

describe("fmtClockDuration", () => {
	test("reads seconds as m:ss and clamps below zero", () => {
		expect(fmtClockDuration(0)).toBe("0:00");
		expect(fmtClockDuration(65.9)).toBe("1:05");
		expect(fmtClockDuration(-3)).toBe("0:00");
		expect(fmtClockDuration(3725)).toBe("62:05");
	});

	test("switches to h:mm from an hour on when asked", () => {
		expect(fmtClockDuration(3599, { hours: true })).toBe("59:59");
		expect(fmtClockDuration(3600, { hours: true })).toBe("1:00");
		expect(fmtClockDuration(3725, { hours: true })).toBe("1:02");
	});
});
