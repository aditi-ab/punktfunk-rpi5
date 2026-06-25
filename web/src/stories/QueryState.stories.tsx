import type { Meta, StoryObj } from '@storybook/react-vite'
import { QueryState } from '@/components/query-state'
import { ApiError } from '@/api/fetcher'

// QueryState is the uniform loading/error wrapper every data-backed route uses —
// the most useful thing to design WITHOUT a running host, since its three states
// (loading spinner / error / unauthorized) never appear together live.
const Loaded = () => (
  <div className="rounded-lg border p-4 text-sm">Loaded content renders here.</div>
)

const meta = {
  title: 'Patterns/QueryState',
  component: QueryState,
  args: { children: <Loaded /> },
} satisfies Meta<typeof QueryState>

export default meta
type Story = StoryObj<typeof meta>

export const Loading: Story = {
  args: { isLoading: true, error: null },
}

export const ErrorWithRetry: Story = {
  args: { isLoading: false, error: new Error('connection refused'), refetch: () => {} },
}

export const Unauthorized: Story = {
  args: { isLoading: false, error: new ApiError(401, null) },
}

export const Loaded_: Story = {
  name: 'Success',
  args: { isLoading: false, error: null },
}
