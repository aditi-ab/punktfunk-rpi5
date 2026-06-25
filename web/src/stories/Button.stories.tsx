import type { Meta, StoryObj } from '@storybook/react-vite'
import { Play } from 'lucide-react'
import { Button } from '@/components/ui/button'

const VARIANTS = ['default', 'secondary', 'outline', 'ghost', 'link', 'destructive'] as const
const SIZES = ['default', 'sm', 'lg', 'icon'] as const

const meta = {
  title: 'UI/Button',
  component: Button,
  args: { children: 'Stream' },
  argTypes: {
    variant: { control: 'select', options: VARIANTS },
    size: { control: 'select', options: SIZES },
    disabled: { control: 'boolean' },
  },
} satisfies Meta<typeof Button>

export default meta
type Story = StoryObj<typeof meta>

/** Playground — drive variant/size/disabled from the Controls panel. */
export const Playground: Story = {}

export const Variants: Story = {
  render: () => (
    <div className="flex flex-wrap items-center gap-3">
      {VARIANTS.map((variant) => (
        <Button key={variant} variant={variant}>
          {variant}
        </Button>
      ))}
    </div>
  ),
}

export const Sizes: Story = {
  render: () => (
    <div className="flex flex-wrap items-center gap-3">
      <Button size="sm">Small</Button>
      <Button size="default">Default</Button>
      <Button size="lg">Large</Button>
      <Button size="icon" aria-label="Play">
        <Play className="size-4" />
      </Button>
    </div>
  ),
}
