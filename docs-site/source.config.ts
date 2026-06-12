import { defineDocs, defineConfig } from 'fumadocs-mdx/config'

// Single docs collection: every .md/.mdx under content/docs/. Frontmatter `title` is
// required (Fumadocs renders it as the page heading); `description` is optional.
export const docs = defineDocs({
  dir: 'content/docs',
})

// Global MDX options (remark/rehype). Default export is read by the fumadocs-mdx vite plugin.
export default defineConfig()
