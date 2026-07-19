import defaultMdxComponents from 'fumadocs-ui/mdx'
import { Tab, Tabs } from 'fumadocs-ui/components/tabs'
import type { MDXComponents } from 'mdx/types'
import BitrateCalculator from '@/components/BitrateCalculator'

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    // Custom components usable in any .md/.mdx without a per-file import.
    BitrateCalculator,
    // Per-platform instructions: <Tabs items={['Linux', 'Windows']}><Tab value="Linux">…
    Tabs,
    Tab,
    ...components,
  } satisfies MDXComponents
}

export const useMDXComponents = getMDXComponents

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>
}
