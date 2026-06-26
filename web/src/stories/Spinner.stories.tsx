import type { Meta, StoryObj } from "@storybook/react-vite";
import { Spinner } from "@/components/ui/spinner";

const meta = {
	title: "UI/Spinner",
	component: Spinner,
} satisfies Meta<typeof Spinner>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};

export const Large: Story = {
	render: () => (
		<div className="flex min-h-60 items-center justify-center">
			<Spinner className="size-40" />
		</div>
	),
};

export const Sizes: Story = {
	render: () => (
		<div className="flex items-center gap-4">
			<Spinner className="size-4" />
			<Spinner className="size-6" />
			<Spinner className="size-10" />
			<Spinner className="size-10 text-primary" />
		</div>
	),
};
