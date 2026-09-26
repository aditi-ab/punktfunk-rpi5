import { type FC, useActionState } from "react";
import { useLocale } from "@/lib/i18n";
import { type LoginError, LoginView } from "./view";

export const SectionLogin: FC<{ next?: string }> = ({ next }) => {
	useLocale();
	// A form action reads the fields from the DOM, so a saved password the browser filled in
	// before hydration counts. A submit before the bundle runs is captured by React's inline
	// runtime and replayed once the form hydrates; nothing ever leaves as a native GET.
	const [error, action, busy] = useActionState<LoginError | null, FormData>(
		async (_prev, data) => {
			let res: Response;
			try {
				res = await fetch("/_auth/login", {
					method: "POST",
					headers: { "Content-Type": "application/json" },
					body: JSON.stringify({
						password: String(data.get("password") ?? ""),
					}),
				});
			} catch {
				return { kind: "wrong" };
			}
			// The throttle locks an IP for anything from a second to five minutes, and it answers
			// BEFORE the password is read — so "wrong password" here would send someone off to
			// re-type one that was never looked at. `Retry-After` is ours, in seconds; check it anyway.
			if (res.status === 429) {
				const wait = Number(res.headers.get("Retry-After"));
				return {
					kind: "throttled",
					seconds: Number.isFinite(wait) && wait > 0 ? wait : 1,
				};
			}
			if (!res.ok) return { kind: "wrong" };
			// Full reload to the target so SSR re-runs WITH the new session cookie. Resolve `next`
			// against our own origin and accept it ONLY if it stays same-origin, rejecting absolute
			// and protocol-relative targets. A naive `!startsWith("//")` check misses `/\evil.com`
			// (browsers fold `\`→`/` under http(s), so it resolves to //evil.com) plus tab/newline
			// tricks; parsing closes them because it's the same parser this redirect will use.
			let safe = "/";
			try {
				const u = new URL(next ?? "/", window.location.origin);
				// `/.//evil.com` parses same-origin but serializes to `//evil.com`.
				if (
					u.origin === window.location.origin &&
					!u.pathname.startsWith("//")
				) {
					safe = u.pathname + u.search + u.hash;
				}
			} catch {
				safe = "/";
			}
			window.location.href = safe;
			return null;
		},
		null,
	);

	return <LoginView action={action} error={error} busy={busy} />;
};
