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
			// Full reload to the target so SSR re-runs WITH the new session cookie. Only a
			// same-origin path — reject protocol-relative/absolute URLs (open-redirect guard).
			const safe =
				next && next.startsWith("/") && !next.startsWith("//") ? next : "/";
			window.location.href = safe;
		} catch {
			setError(true);
			setBusy(false);
		}
	};

	return <LoginView onSubmit={onSubmit} error={error} busy={busy} />;
};
