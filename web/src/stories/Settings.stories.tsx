import type { Meta, StoryObj } from '@storybook/react-vite'
import { SettingsPage } from '@/routes/settings'

// Settings reads no API (just the locale + a logout button), so it renders
// directly — no mock needed.
const meta = {
  title: 'Pages/Settings',
  component: SettingsPage,
} satisfies Meta<typeof SettingsPage>

export default meta
type Story = StoryObj<typeof meta>

export const Default: Story = {}
