import { type FC, useState } from "react";
import { useLocale } from "@/lib/i18n";
import { LoginView } from "./view";

export const SectionLogin: FC<{ next?: string }> = ({ next }) => {
	useLocale();
	const [error, setError] = useState(false);
	const [busy, setBusy] = useState(false);

	const onSubmit = async (password: string) => {
		setBusy(true);
		setError(false);
		try {
			const res = await fetch("/_auth/login", {
				method: "POST",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({ password }),
			});
			if (!res.ok) {
				setError(true);
				setBusy(false);
				return;
			}
			// Full reload to the target so SSR re-runs WITH the new session cookie. Resolve `next`
			// against our own origin and accept it ONLY if it stays same-origin, rejecting absolute
			// and protocol-relative targets. A naive `!startsWith("//")` check misses `/\evil.com`
			// (browsers fold `\`→`/` under http(s), so it resolves to //evil.com) plus tab/newline
			// tricks; parsing closes them because it's the same parser this redirect will use.
			let safe = "/";
			try {
				const u = new URL(next ?? "/", window.location.origin);
				if (u.origin === window.location.origin) {
					safe = u.pathname + u.search + u.hash;
				}
			} catch {
				safe = "/";
			}
			window.location.href = safe;
		} catch {
			setError(true);
			setBusy(false);
		}
	};

	return <LoginView onSubmit={onSubmit} error={error} busy={busy} />;
};
