import type { Meta, StoryObj } from '@storybook/react-vite'
import { Dashboard } from '@/routes/index'
import { MockApi } from './lib/mock-api'
import { statusActive, statusIdle } from './lib/fixtures'

const meta = {
  title: 'Pages/Dashboard',
  component: Dashboard,
} satisfies Meta<typeof Dashboard>

export default meta
type Story = StoryObj<typeof meta>

export const ActiveSession: Story = {
  render: () => (
    <MockApi routes={{ '/api/v1/status': statusActive }}>
      <Dashboard />
    </MockApi>
  ),
}

export const Idle: Story = {
  render: () => (
    <MockApi routes={{ '/api/v1/status': statusIdle }}>
      <Dashboard />
    </MockApi>
  ),
}
