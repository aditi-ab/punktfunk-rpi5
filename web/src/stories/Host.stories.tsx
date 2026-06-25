import type { Meta, StoryObj } from '@storybook/react-vite'
import { HostPage } from '@/routes/host'
import { MockApi } from './lib/mock-api'
import { compositors, hostInfo } from './lib/fixtures'

const meta = {
  title: 'Pages/Host',
  component: HostPage,
} satisfies Meta<typeof HostPage>

export default meta
type Story = StoryObj<typeof meta>

export const Default: Story = {
  render: () => (
    <MockApi routes={{ '/api/v1/host': hostInfo, '/api/v1/compositors': compositors }}>
      <HostPage />
    </MockApi>
  ),
}
