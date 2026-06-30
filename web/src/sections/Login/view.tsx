import { ease } from "@unom/style";
import { motion } from "motion/react";
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
		<div className="flex flex-col min-h-screen items-center justify-center p-6">
			<motion.div
				transition={ease.quint(0.9).out}
				variants={{ enter: { scale: 1, y: 0 }, from: { scale: 0, y: 100 } }}
				className="mb-8 flex w-[120px]"
			>
				<Logo />
			</motion.div>
			<Card className="w-full max-w-sm h-fit grow-0">
				<CardHeader className="items-start text-left">
					<CardTitle className="text-xl">{m.login_title()}</CardTitle>
					<p className="text-sm text-muted-foreground">
						{m.login_subtitle()}{" "}
						<a
							href="https://docs.punktfunk.unom.io/docs/forgot-password"
							target="_blank"
							rel="noreferrer"
							className="underline underline-offset-4 hover:text-foreground"
						>
							{m.login_docs_link()}
						</a>
					</p>
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
