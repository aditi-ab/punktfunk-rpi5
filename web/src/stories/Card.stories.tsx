import type { Meta, StoryObj } from "@storybook/react-vite";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
	Card,
	CardContent,
	CardDescription,
	CardFooter,
	CardHeader,
	CardTitle,
} from "@/components/ui/card";

const meta = {
	title: "UI/Card",
	component: Card,
	// Card requires `children`; every story supplies its own via `render`, so this
	// is just a placeholder to satisfy the arg type.
	args: { children: null },
} satisfies Meta<typeof Card>;

export default meta;
type Story = StoryObj<typeof meta>;

export const HostCard: Story = {
	render: () => (
		<Card className="max-w-sm">
			<CardHeader>
				<div className="flex items-center justify-between">
					<CardTitle>ENRICOS-DESKTOP</CardTitle>
					<Badge variant="success">online</Badge>
				</div>
				<CardDescription>RTX 5070 Ti · NVENC · 5120×1440 @ 240</CardDescription>
			</CardHeader>
			<CardContent className="text-sm text-muted-foreground">
				Paired 2 days ago. Last session 11 ms p50 capture→present.
			</CardContent>
			<CardFooter className="gap-2">
				<Button size="sm">Connect</Button>
				<Button size="sm" variant="outline">
					Details
				</Button>
			</CardFooter>
		</Card>
	),
};
