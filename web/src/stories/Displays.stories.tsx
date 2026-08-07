import type { Meta, StoryObj } from "@storybook/react-vite";
import { useState } from "react";
import type { DisplayPolicy } from "@/api/gen/model/displayPolicy";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { m } from "@/paraglide/messages";
import { DisplayForm } from "@/sections/Displays/DisplayCard";
import {
	displayCustomPresets,
	displayEffective,
	displayPolicy,
	displayPresets,
} from "./lib/fixtures";

/**
 * The **Virtual displays** policy form — the console's largest configuration surface, and until now
 * the only page with no story at all. That gap is why a real regression shipped unseen: the preset
 * tiles are cards nested INSIDE the page's config card, so their motion parent is that card rather
 * than the page's `<Section>`, and a card sets no `delayChildren` — every tile landed on the same
 * frame while every other grid in the console staggered. It is invisible in a diff and invisible to
 * `tsc`; only a rendered page shows it.
 *
 * So the `<Card>` wrapper below is NOT decoration. It reproduces the page's motion nesting, which is
 * the thing under test — dropping it would make the story pass for the wrong reason.
 */
const Harness = ({ seed }: { seed: DisplayPolicy }) => {
	const [draft, setDraft] = useState<DisplayPolicy>(seed);
	return (
		<Card>
			<CardHeader>
				<CardTitle>{m.display_config_title()}</CardTitle>
			</CardHeader>
			<CardContent className="space-y-4">
				<DisplayForm
					draft={draft}
					setDraft={setDraft}
					presets={displayPresets}
					customPresets={displayCustomPresets}
					serverEffective={displayEffective}
					serverCaptureMonitor={() => null}
					apply={setDraft}
					applyAxis={(patch) => setDraft({ ...draft, ...patch })}
					saveDraft={() => {}}
					busy={false}
					dirty={false}
					revert={() => {}}
				/>
			</CardContent>
		</Card>
	);
};

const meta = {
	title: "Pages/Displays",
	component: Harness,
	args: { seed: displayPolicy },
} satisfies Meta<typeof Harness>;

export default meta;
type Story = StoryObj<typeof meta>;

/** A host sitting on a built-in preset — the tiles, and the operator's saved bundles below them. */
export const Default: Story = {};

/** "Custom" reveals every axis by hand: the long form under the tiles. */
export const CustomFields: Story = {
	args: { seed: { ...displayPolicy, preset: "custom" } },
};

/** A fresh host has saved no bundles of its own — the custom rail collapses to just "Save as". */
export const NoCustomPresets: Story = {
	args: { seed: displayPolicy },
	render: (args) => (
		<Card>
			<CardHeader>
				<CardTitle>{m.display_config_title()}</CardTitle>
			</CardHeader>
			<CardContent className="space-y-4">
				<DisplayForm
					draft={args.seed}
					setDraft={() => {}}
					presets={displayPresets}
					customPresets={[]}
					serverEffective={displayEffective}
					serverCaptureMonitor={() => null}
					apply={() => {}}
					applyAxis={() => {}}
					saveDraft={() => {}}
					busy={false}
					dirty={false}
					revert={() => {}}
				/>
			</CardContent>
		</Card>
	),
};
