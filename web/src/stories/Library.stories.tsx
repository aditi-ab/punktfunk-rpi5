import type { Meta, StoryObj } from "@storybook/react-vite";
import { GameForm } from "@/sections/Library/GameForm";
import { LibraryGrid } from "@/sections/Library/LibraryGrid";
import { library } from "./lib/fixtures";

const noop = () => {};
const idle = { isLoading: false, error: null, refetch: noop };
const emptyForm = {
	title: "",
	portrait: "",
	hero: "",
	header: "",
	command: "",
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
			isDeleting={false}
		/>
	),
};

export const Empty: Story = {
	render: () => (
		<LibraryGrid
			library={{ data: [], ...idle }}
			onEdit={noop}
			onDelete={noop}
			isDeleting={false}
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
