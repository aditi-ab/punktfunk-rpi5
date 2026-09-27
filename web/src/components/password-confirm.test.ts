import { expect, test } from "bun:test";
import { ApiError } from "@/api/fetcher";
import { passwordFailure } from "./password-confirm";

test("a gated call's refusal is wrong or throttled, nothing else", () => {
	expect(passwordFailure(new ApiError(401, null))).toBe("wrong");
	expect(passwordFailure(new ApiError(429, null))).toBe("throttled");
	expect(passwordFailure(new ApiError(503, null))).toBeNull();
	expect(passwordFailure(new ApiError(409, null))).toBeNull();
	expect(passwordFailure(new TypeError("offline"))).toBeNull();
});
