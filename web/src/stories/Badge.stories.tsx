import type { Meta, StoryObj } from "@storybook/react-vite";
import { Badge } from "@/components/ui/badge";

const VARIANTS = [
	"default",
	"secondary",
	"success",
	"warning",
	"destructive",
	"outline",
] as const;

const meta = {
	title: "UI/Badge",
	component: Badge,
	args: { children: "badge" },
	argTypes: {
		variant: { control: "select", options: VARIANTS },
	},
} satisfies Meta<typeof Badge>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Playground: Story = {};

export const All: Story = {
	render: () => (
		<div className="flex flex-wrap items-center gap-2">
			{VARIANTS.map((variant) => (
				<Badge key={variant} variant={variant}>
					{variant}
				</Badge>
			))}
		</div>
	),
};
