import { expect, test } from "bun:test";
import { apiFetch } from "./fetcher";

test("redirects only the auth middleware's 401", async () => {
	const originalFetch = globalThis.fetch;
	const originalSetTimeout = globalThis.setTimeout;
	const originalWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
	let responseBody: unknown = {
		statusCode: 401,
		statusMessage: "password confirmation failed",
	};
	let redirects = 0;

	globalThis.fetch = (async () =>
		new Response(JSON.stringify(responseBody), {
			status: 401,
			statusText: "Unauthorized",
		})) as typeof fetch;
	globalThis.setTimeout = ((_: () => void) => {
		redirects += 1;
		return 1;
	}) as typeof setTimeout;
	Object.defineProperty(globalThis, "window", {
		configurable: true,
		value: {
			location: {
				pathname: "/automation",
				search: "",
				hash: "",
				href: "/automation",
			},
		},
	});

	try {
		await expect(apiFetch("/api/v1/hooks")).rejects.toMatchObject({
			status: 401,
		});
		expect(redirects).toBe(0);

		responseBody = { error: "unauthorized" };
		await expect(apiFetch("/api/v1/hooks")).rejects.toMatchObject({
			status: 401,
		});
		expect(redirects).toBe(1);
	} finally {
		globalThis.fetch = originalFetch;
		globalThis.setTimeout = originalSetTimeout;
		if (originalWindow) {
			Object.defineProperty(globalThis, "window", originalWindow);
		} else {
			Reflect.deleteProperty(globalThis, "window");
		}
	}
});
