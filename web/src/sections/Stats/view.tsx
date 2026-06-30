import Section from "@unom/ui/section";
import type { FC, ReactNode } from "react";
import { m } from "@/paraglide/messages";

/**
 * The Performance page LAYOUT — the single source of how the cards stack. Both the live page
 * (`index.tsx`, slots = the self-contained `*Section` containers) and Storybook (slots = the pure
 * cards with mock state) fill these slots, so the arrangement can never drift between them. `live`
 * and `detail` are nullable slots — the page passes them only when armed / a recording is selected.
 */
export const StatsView: FC<{
	control: ReactNode;
	live: ReactNode;
	recordings: ReactNode;
	detail: ReactNode;
}> = ({ control, live, recordings, detail }) => (
	<Section maxWidth={false}>
		<div className="flex flex-col gap-card">
			<div className="space-y-1">
				<h1 className="text-2xl font-semibold">{m.stats_title()}</h1>
				<p className="text-sm text-muted-foreground">{m.stats_subtitle()}</p>
			</div>

			{control}
			{live}
			{recordings}
			{detail}
		</div>
	</Section>
);
