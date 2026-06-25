import type { StorybookConfig } from '@storybook/react-vite'

const config: StorybookConfig = {
  stories: ['../src/**/*.stories.@(ts|tsx)'],
  addons: [],
  framework: {
    name: '@storybook/react-vite',
    options: {
      // Use the slim, Start/Nitro-free Vite config (see vite.storybook.config.ts).
      builder: { viteConfigPath: './vite.storybook.config.ts' },
    },
  },
}

export default config
