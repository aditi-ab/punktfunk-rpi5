import type { Meta, StoryObj } from "@storybook/react-vite";
import { BrandMark } from "@/components/brand-mark";
import { Logo } from "@/components/logo";
import { Wordmark } from "@/components/wordmark";

const meta = {
	title: "Brand/Marks",
	component: BrandMark,
} satisfies Meta<typeof BrandMark>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Mark: Story = {
	render: () => (
		<div className="flex items-end gap-6">
			<BrandMark className="size-8" />
			<BrandMark className="size-12" />
			<BrandMark className="size-20" />
		</div>
	),
};

export const Word: Story = {
	render: () => (
		<div className="space-y-4">
			<Wordmark className="h-4" />
			<Wordmark className="h-6 text-foreground" />
			<Wordmark className="h-8 text-primary" />
		</div>
	),
};

export const Lockup: Story = {
	render: () => (
		<div className="pl-8 pt-6">
			<Logo className="w-48" />
		</div>
	),
};
