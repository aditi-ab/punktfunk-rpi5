import type { Meta, StoryObj } from "@storybook/react-vite";
import { LoginView } from "@/sections/Login/view";

const meta = {
	title: "Pages/Login",
	component: LoginView,
	parameters: { layout: "fullscreen" },
	args: { onSubmit: () => {}, error: false, busy: false },
} satisfies Meta<typeof LoginView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};

export const Error: Story = { args: { error: true } };
