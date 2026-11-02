// A seat's held log lines across quiet polls, re-reads and restarts of its host.
import { expect, test } from "bun:test";
import type { LogEntry } from "@/api/gen/model/logEntry";
import { appendSeatPage } from "./rows";

const run = (start: number, from: number, to: number): LogEntry[] =>
	Array.from({ length: to - from + 1 }, (_, i) => ({
		seq: from + i,
		ts_ms: start + from + i,
		level: "INFO",
		target: "seat",
		msg: `line ${from + i}`,
	}));

test("a quiet poll keeps every line", () => {
	const held = run(1_000, 1, 3);
	expect(appendSeatPage(held, [])).toBe(held);
});

test("a page goes on after what is held", () => {
	expect(appendSeatPage(run(1_000, 1, 3), run(1_000, 4, 5))).toEqual(
		run(1_000, 1, 5),
	);
});

test("re-reading the same ring from its start adds only what is new", () => {
	expect(appendSeatPage(run(1_000, 1, 3), run(1_000, 1, 4))).toEqual(
		run(1_000, 1, 4),
	);
	expect(appendSeatPage(run(1_000, 1, 9), run(1_000, 1, 4))).toEqual(
		run(1_000, 1, 9),
	);
});

test("a restarted host's lines go after the earlier run's", () => {
	const crashed = run(1_000, 1, 40);
	const after = appendSeatPage(crashed, run(9_000, 1, 10));
	expect(after).toEqual([...crashed, ...run(9_000, 1, 10)]);
	// The next run's own re-read finds its lines, not the crashed run's.
	expect(appendSeatPage(after, run(9_000, 1, 12))).toEqual([
		...after,
		...run(9_000, 11, 12),
	]);
});
