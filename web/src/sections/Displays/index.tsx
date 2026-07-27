import Section from "@unom/ui/section";
import type { FC } from "react";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { DisplaySection } from "./DisplayCard";
import { MonitorCard } from "./MonitorCard";
import { SessionGameCard } from "./SessionGameCard";

/**
 * The **Virtual displays** page (design/display-management.md): the host's virtual-display policy
 * (presets + every axis) plus the live-display list + multi-monitor arrangement. Its own nav
 * section — the config surface is large enough to warrant the room, and it kept the Host page busy.
 *
 * The session⇄game lifetime card sits here rather than on its own page because it is the same
 * question one step further out: keep-alive decides how long a *display* outlives a disconnect, and
 * this decides whether the *game* does.
 */
export const SectionDisplays: FC = () => {
	useLocale();
	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<h1 className="text-2xl font-semibold">{m.nav_displays()}</h1>
				<DisplaySection />
				<MonitorCard />
				<SessionGameCard />
			</div>
		</Section>
	);
};
