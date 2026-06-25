import type { Meta, StoryObj } from '@storybook/react-vite'
import { LibraryPage } from '@/routes/library'
import { MockApi } from './lib/mock-api'
import { library } from './lib/fixtures'

const meta = {
  title: 'Pages/Library',
  component: LibraryPage,
} satisfies Meta<typeof LibraryPage>

export default meta
type Story = StoryObj<typeof meta>

export const Populated: Story = {
  render: () => (
    <MockApi routes={{ '/api/v1/library': library }}>
      <LibraryPage />
    </MockApi>
  ),
}

export const Empty: Story = {
  render: () => (
    <MockApi routes={{ '/api/v1/library': [] }}>
      <LibraryPage />
    </MockApi>
  ),
}
