import Section from "@unom/ui/section";
import type { FC } from "react";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { DisplaySection } from "./DisplayCard";

/**
 * The **Virtual displays** page (design/display-management.md): the host's virtual-display policy
 * (presets + every axis) plus the live-display list + multi-monitor arrangement. Its own nav
 * section — the config surface is large enough to warrant the room, and it kept the Host page busy.
 */
export const SectionDisplays: FC = () => {
	useLocale();
	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<h1 className="text-2xl font-semibold">{m.nav_displays()}</h1>
				<DisplaySection />
			</div>
		</Section>
	);
};
