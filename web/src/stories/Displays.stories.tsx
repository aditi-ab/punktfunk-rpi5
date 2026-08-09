import type { Meta, StoryObj } from "@storybook/react-vite";
import { useState } from "react";
import { userEvent, within } from "storybook/test";
import type { DisplayPolicy } from "@/api/gen/model/displayPolicy";
import { DisplayForm, DisplayTabs } from "@/sections/Displays/DisplayCard";
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
 * So the wrapper below is NOT decoration. It reproduces the page's motion nesting, which is the
 * thing under test — dropping it would make the story pass for the wrong reason. It renders the
 * page's real `DisplayTabs` shell for exactly that reason: the tabs sit between the page `<Section>`
 * and the card, so they are part of the ancestor chain this story exists to pin.
 */
const Harness = ({
	seed,
	dirty = false,
}: {
	seed: DisplayPolicy;
	dirty?: boolean;
}) => {
	const [draft, setDraft] = useState<DisplayPolicy>(seed);
	return (
		<DisplayTabs
			dirty={dirty}
			live={
				<p className="text-sm text-muted-foreground">
					The live list reads `/display/state`, so it is not part of this story
					— see the tab strip and the Configuration pane.
				</p>
			}
			configuration={
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
					dirty={dirty}
					revert={() => {}}
				/>
			}
		/>
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
		<DisplayTabs
			dirty={false}
			live={null}
			configuration={
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
			}
		/>
	),
};

/**
 * Unsaved Custom edits, with the Configuration tab NOT open.
 *
 * The dirty marker has to survive being on the other tab — the whole reason it moved off the card
 * header and onto the trigger. If this story ever shows a bare "Configuration" label, the warning
 * has gone silent exactly when it matters most.
 */
export const UnsavedOnOtherTab: Story = {
	args: { seed: { ...displayPolicy, preset: "custom" }, dirty: true },
	play: async ({ canvasElement }) => {
		const canvas = within(canvasElement);
		await userEvent.click(await canvas.findByRole("tab", { name: /Live/i }));
	},
};
