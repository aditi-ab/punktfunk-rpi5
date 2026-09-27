import { expect, test } from "bun:test";
import { ApiError } from "@/api/fetcher";
import { apiErrorMessage } from "./errors";

test("reads the host's error and the BFF's thrown refusal alike", () => {
	expect(apiErrorMessage(new ApiError(409, { error: "busy" }))).toBe("busy");
	const thrown = { error: true, statusCode: 503, statusMessage: "no token" };
	expect(apiErrorMessage(new ApiError(503, thrown, ""))).toBe("no token");
	expect(apiErrorMessage(new ApiError(500, "oops", "Server Error"))).toBe(
		"Server Error",
	);
});
