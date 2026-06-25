import type { Meta, StoryObj } from '@storybook/react-vite'
import { ClientsPage } from '@/routes/clients'
import { MockApi } from './lib/mock-api'
import { pairedClients } from './lib/fixtures'

const meta = {
  title: 'Pages/Clients',
  component: ClientsPage,
} satisfies Meta<typeof ClientsPage>

export default meta
type Story = StoryObj<typeof meta>

export const Paired: Story = {
  render: () => (
    <MockApi routes={{ '/api/v1/clients': pairedClients }}>
      <ClientsPage />
    </MockApi>
  ),
}

export const Empty: Story = {
  render: () => (
    <MockApi routes={{ '/api/v1/clients': [] }}>
      <ClientsPage />
    </MockApi>
  ),
}
