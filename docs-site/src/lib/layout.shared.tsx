import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared'

// Shared chrome (nav title, links) for both the docs layout and the home layout.
export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: 'punktfunk',
    },
  }
}
