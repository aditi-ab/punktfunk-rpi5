import type { Meta, StoryObj } from "@storybook/react-vite";
import { LibraryView } from "@/sections/Library/view";
import { library } from "./lib/fixtures";

const meta = {
	title: "Pages/Library",
	component: LibraryView,
	args: {
		onCreate: () => Promise.resolve(),
		onUpdate: () => Promise.resolve(),
		onDelete: () => Promise.resolve(),
		isSaving: false,
		isDeleting: false,
	},
} satisfies Meta<typeof LibraryView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Populated: Story = {
	args: { library: { data: library, isLoading: false, error: null } },
};

export const Empty: Story = {
	args: { library: { data: [], isLoading: false, error: null } },
};
