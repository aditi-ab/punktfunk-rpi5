import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared'
import BrandMark from '@/components/BrandMark'

// Shared chrome (nav title, links) for both the docs layout and the home layout.
// The lens mark + wordmark mirror the punktfunk marketing site's header.
export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: (
        <>
          <BrandMark className="size-6" />
          <span className="font-display font-semibold tracking-tight">punktfunk</span>
        </>
      ),
    },
    links: [
      { text: 'Docs', url: '/docs' },
      { text: 'Website', url: 'https://punktfunk.unom.io' },
    ],
  }
}
