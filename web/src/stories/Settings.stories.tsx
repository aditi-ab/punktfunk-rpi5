import type { Meta, StoryObj } from "@storybook/react-vite";
import { SectionSettings } from "@/sections/Settings";

const meta = {
	title: "Pages/Settings",
	component: SectionSettings,
} satisfies Meta<typeof SectionSettings>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};
