import type { Meta, StoryObj } from "@storybook/react-vite";
import { GameForm } from "@/sections/Library/GameForm";
import { LibraryGrid } from "@/sections/Library/LibraryGrid";
import { SourceToggles } from "@/sections/Library/SourceToggles";
import { library } from "./lib/fixtures";

const noop = () => {};
const idle = { isLoading: false, error: null, refetch: noop };
const emptyForm = {
	title: "",
	portrait: "",
	hero: "",
	header: "",
	logo: "",
	command: "",
	platform: "",
	description: "",
	developer: "",
	publisher: "",
	releaseYear: "",
	genres: "",
	tags: "",
	region: "",
	players: "",
};

// The overview grid and the add/edit form are separate components now, so the stories
// render each on its own (no combined page view).
const meta = {
	title: "Pages/Library",
	parameters: { layout: "padded" },
} satisfies Meta;

export default meta;
type Story = StoryObj;

export const Populated: Story = {
	render: () => (
		<LibraryGrid
			library={{ data: library, ...idle }}
			onEdit={noop}
			onDelete={noop}
			deletingId={null}
		/>
	),
};

export const Empty: Story = {
	render: () => (
		<LibraryGrid
			library={{ data: [], ...idle }}
			onEdit={noop}
			onDelete={noop}
			deletingId={null}
		/>
	),
};

export const Sources: Story = {
	render: () => (
		<SourceToggles
			// A Linux host's scanner set, one turned off — the widest built-in list.
			scanners={[
				{ id: "steam", label: "Steam", enabled: true },
				{ id: "lutris", label: "Lutris", enabled: false },
				{ id: "heroic", label: "Heroic (Epic / GOG / Amazon)", enabled: true },
			]}
			busyId={null}
			onToggle={noop}
		/>
	),
};

export const AddForm: Story = {
	render: () => (
		<GameForm
			initial={emptyForm}
			mode="add"
			onSubmit={noop}
			onCancel={noop}
			isSaving={false}
		/>
	),
};
