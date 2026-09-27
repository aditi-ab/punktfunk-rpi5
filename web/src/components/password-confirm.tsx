// The console-password field every gated action shows, and the two refusals the BFF gives it
// (server/util/confirm.ts): 401 when the password is wrong, 429 when the per-peer budget is spent.
import { type FC, type ReactNode, useCallback, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { m } from "@/paraglide/messages";

export type PasswordFailure = "wrong" | "throttled" | null;

/** The password refusal behind `e`, or null for any other error. */
export function passwordFailure(e: unknown): PasswordFailure {
	if (!(e instanceof ApiError)) return null;
	if (e.status === 401) return "wrong";
	if (e.status === 429) return "throttled";
	return null;
}

/**
 * The last attempt's refusal. `classify(e)` records it and returns false for any other error, so
 * the caller keeps its own failure message for those. Both callbacks are stable.
 */
export function usePasswordFailure() {
	const [failure, setFailure] = useState<PasswordFailure>(null);
	const reset = useCallback(() => setFailure(null), []);
	const classify = useCallback((e: unknown): boolean => {
		const f = passwordFailure(e);
		setFailure(f);
		return f !== null;
	}, []);
	return { failure, reset, classify };
}

export const PasswordConfirmField: FC<{
	id: string;
	value: string;
	onChange: (v: string) => void;
	failure: PasswordFailure;
	help?: ReactNode;
	autoFocus?: boolean;
}> = ({ id, value, onChange, failure, help, autoFocus }) => (
	<div className="space-y-2">
		<Label htmlFor={id}>{m.store_spec_password()}</Label>
		<Input
			id={id}
			type="password"
			autoComplete="current-password"
			autoFocus={autoFocus}
			value={value}
			onChange={(e) => onChange(e.target.value)}
		/>
		{help && <p className="text-xs text-muted-foreground">{help}</p>}
		{failure && (
			<p role="alert" className="text-xs text-destructive">
				{failure === "wrong"
					? m.update_apply_wrong_password()
					: m.update_apply_throttled()}
			</p>
		)}
	</div>
);
