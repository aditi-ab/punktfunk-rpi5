import Section from "@unom/ui/section";
import type { FC, ReactNode } from "react";
import { m } from "@/paraglide/messages";

/**
 * The Logs page LAYOUT — the live page (`index.tsx`) and the Storybook story fill the single
 * `viewer` slot, so the arrangement can never drift between them (same pattern as StatsView).
 */
export const LogsView: FC<{ viewer: ReactNode }> = ({ viewer }) => (
	<Section maxWidth={false}>
		<div className="flex flex-col gap-card">
			<div className="space-y-1">
				<h1 className="text-2xl font-semibold">{m.logs_title()}</h1>
				<p className="text-sm text-muted-foreground">{m.logs_subtitle()}</p>
			</div>

			{viewer}
		</div>
	</Section>
);
