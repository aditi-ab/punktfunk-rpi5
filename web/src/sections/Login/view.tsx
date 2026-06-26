import { type FC, useState } from "react";
import Logo from "@/components/logo";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { m } from "@/paraglide/messages";

export const LoginView: FC<{
	onSubmit: (password: string) => void;
	error: boolean;
	busy: boolean;
}> = ({ onSubmit, error, busy }) => {
	const [password, setPassword] = useState("");
	return (
		<div className="flex min-h-screen items-center justify-center p-6">
			<Card className="w-full max-w-sm">
				<CardHeader className="items-center text-center">
					<div className="mb-2 flex w-[80px] items-center gap-2">
						<Logo />
					</div>
					<CardTitle>{m.login_title()}</CardTitle>
					<p className="text-sm text-muted-foreground">{m.login_subtitle()}</p>
				</CardHeader>
				<CardContent>
					<form
						onSubmit={(e) => {
							e.preventDefault();
							onSubmit(password);
						}}
						className="space-y-4"
					>
						<div className="space-y-2">
							<Label htmlFor="pw">{m.login_password()}</Label>
							<Input
								id="pw"
								type="password"
								// biome-ignore lint/a11y/noAutofocus: the login screen is the sole focus target.
								autoFocus
								autoComplete="current-password"
								value={password}
								onChange={(e) => setPassword(e.target.value)}
							/>
						</div>
						{error && (
							<p className="text-sm text-destructive">{m.login_error()}</p>
						)}
						<Button
							type="submit"
							className="w-full"
							disabled={busy || !password}
						>
							{busy ? m.login_signing_in() : m.login_submit()}
						</Button>
					</form>
				</CardContent>
			</Card>
		</div>
	);
};
