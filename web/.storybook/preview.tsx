// Import the console's REAL stylesheet directly (rememed-style) — the @theme
// blocks process because this is the literal entry Storybook's Vite pipeline sees.
import '../src/styles.css'
// The console loads its brand typeface separately (in __root.tsx); do the same
// here or every story falls back to system-ui and looks off.
import '@fontsource-variable/geist'
import { useEffect } from 'react'
import { definePreview } from '@storybook/react-vite'
import { MaterialProvider, defaultMaterialTheme } from '@unom/ui/material'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'

// React Query is present so any query-backed component mounts without a real
// host. Stories should feed mock data rather than fetch — retries are off so a
// stray request fails fast instead of hanging the canvas.
const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
})

export default definePreview({
  addons: [],
  // The live console pins dark; default the canvas to dark too, with a toolbar
  // switch to preview the light theme while designing.
  initialGlobals: { theme: 'dark' },
  globalTypes: {
    theme: {
      description: 'Light/dark color scheme',
      toolbar: {
        title: 'Theme',
        icon: 'circlehollow',
        items: [
          { value: 'dark', icon: 'moon', title: 'Dark' },
          { value: 'light', icon: 'sun', title: 'Light' },
        ],
        dynamicTitle: true,
      },
    },
  },
  decorators: [
    (Story, context) => {
      const dark = (context.globals.theme as string) !== 'light'
      // `layout: 'fullscreen'` stories (e.g. the AppShell) own their own padding;
      // everything else gets a comfortable inset.
      const fullscreen = context.parameters.layout === 'fullscreen'
      // Mirror `.dark` onto <html> so the body's token-driven background AND any
      // portal-mounted content (radix dialogs, popovers) pick up the right
      // palette — the console keys its whole token set off `html.dark`.
      useEffect(() => {
        document.documentElement.classList.toggle('dark', dark)
      }, [dark])
      return (
        <QueryClientProvider client={queryClient}>
          <MaterialProvider theme={defaultMaterialTheme}>
            <div className={dark ? 'dark' : ''}>
              <div
                className={`min-h-screen bg-background text-foreground ${fullscreen ? '' : 'p-6'}`}
              >
                <Story />
              </div>
            </div>
          </MaterialProvider>
        </QueryClientProvider>
      )
    },
  ],
  parameters: {
    controls: { matchers: { color: /(background|color)$/i, date: /Date$/ } },
    layout: 'padded',
  },
})
