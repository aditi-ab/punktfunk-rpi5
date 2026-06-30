import Section from "@unom/ui/section";
import { LogOut } from "lucide-react";
import type { FC } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { changeLocale, type Locale, locales, useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";

// Settings reads no API (just the locale + a logout button), so it's a single
// presentational section — no container/view split needed.
export const SectionSettings: FC = () => {
	const current = useLocale();

	const onLogout = async () => {
		await fetch("/_auth/logout", { method: "POST" });
		window.location.href = "/login";
	};

	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<h1 className="text-2xl font-semibold">{m.settings_title()}</h1>

				<Card className="max-w-lg">
					<CardHeader>
						<CardTitle>{m.settings_language()}</CardTitle>
					</CardHeader>
					<CardContent className="flex gap-2">
						{locales.map((l: Locale) => (
							<Button
								key={l}
								variant={l === current ? "default" : "outline"}
								size="sm"
								className="uppercase"
								onClick={() => changeLocale(l)}
							>
								{l}
							</Button>
						))}
					</CardContent>
				</Card>

				<Card className="max-w-lg">
					<CardHeader>
						<CardTitle>{m.nav_settings()}</CardTitle>
					</CardHeader>
					<CardContent>
						<Button variant="outline" onClick={onLogout}>
							<LogOut className="size-4" />
							{m.action_logout()}
						</Button>
					</CardContent>
				</Card>
			</div>
		</Section>
	);
};
