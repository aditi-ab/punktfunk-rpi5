import type { Meta, StoryObj } from '@storybook/react-vite'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'

const meta = {
  title: 'UI/Inputs',
  component: Input,
} satisfies Meta<typeof Input>

export default meta
type Story = StoryObj<typeof meta>

export const Form: Story = {
  render: () => (
    <div className="max-w-sm space-y-4">
      <div className="space-y-1.5">
        <Label htmlFor="host">Host address</Label>
        <Input id="host" placeholder="192.168.1.173" />
      </div>
      <div className="space-y-1.5">
        <Label htmlFor="pin">Pairing PIN</Label>
        <Input id="pin" inputMode="numeric" maxLength={4} placeholder="0000" />
      </div>
      <div className="space-y-1.5">
        <Label htmlFor="disabled">Disabled</Label>
        <Input id="disabled" disabled placeholder="unavailable" />
      </div>
    </div>
  ),
}
