// The wire schemas must decode EXACTLY what the host emits — the JSON literals here are the
// Rust side's snapshot-test strings (crates/punktfunk-host/src/events.rs), the schema gate.
import { describe, expect, test } from "bun:test";
import { decodeHostEvent, kindMatches } from "../src/wire.js";

describe("wire", () => {
	test("decodes the host's snapshot frames", () => {
		const stream = decodeHostEvent(
			JSON.parse(
				'{"seq":4182,"ts_ms":1700000000000,"schema":1,"kind":"stream.started","stream":{"mode":"3840x2160@120","hdr":true,"client":"Living Room TV","app":"steam:570","plane":"native"}}',
			),
		);
		expect(stream._tag).toBe("Right");
		if (stream._tag === "Right" && stream.right.kind === "stream.started") {
			expect(stream.right.stream.mode).toBe("3840x2160@120");
			expect(stream.right.stream.app).toBe("steam:570");
		}

		const disc = decodeHostEvent(
			JSON.parse(
				'{"seq":1,"ts_ms":1700000000000,"schema":1,"kind":"client.disconnected","client":{"name":"Deck","fingerprint":"b1c2","plane":"gamestream"},"reason":"timeout"}',
			),
		);
		expect(disc._tag).toBe("Right");
		if (disc._tag === "Right" && disc.right.kind === "client.disconnected") {
			expect(disc.right.reason).toBe("timeout");
		}

		const stopping = decodeHostEvent(
			JSON.parse('{"seq":2,"ts_ms":1700000000000,"schema":1,"kind":"host.stopping"}'),
		);
		expect(stopping._tag).toBe("Right");
	});

	test("tolerates unknown keys (additive-only wire)", () => {
		const r = decodeHostEvent({
			seq: 9,
			ts_ms: 1,
			schema: 1,
			kind: "library.changed",
			source: "manual",
			future_field: { anything: true },
		});
		expect(r._tag).toBe("Right");
	});

	test("unknown kinds fail decode (they ride the raw channel)", () => {
		const r = decodeHostEvent({
			seq: 9,
			ts_ms: 1,
			schema: 1,
			kind: "totally.new",
		});
		expect(r._tag).toBe("Left");
	});

	test("kindMatches mirrors the host filter semantics", () => {
		expect(kindMatches("stream.started", "stream.started")).toBe(true);
		expect(kindMatches("stream.*", "stream.stopped")).toBe(true);
		expect(kindMatches("stream.*", "streamx.started")).toBe(false);
		expect(kindMatches("stream.started", "stream.stopped")).toBe(false);
	});
});
