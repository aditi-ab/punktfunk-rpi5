// The docs reuse the punktfunk footer from the shared unom CMS (cms.unom.io).
// The footer shape comes from @unom/app-ui/footer so the docs and the marketing
// site share one type. The CMS is multi-tenant: footer is a per-tenant
// collection, so scope the read to this project's tenant. Read-only GET, so a
// plain typed fetch rather than pulling in the Payload SDK + generated types.
import type { FooterData } from '@unom/app-ui/footer'

const CMS_URL = 'https://cms.unom.io'

// This project's tenant in the shared CMS.
const TENANT = 'punktfunk'

export type { FooterData as Footer } from '@unom/app-ui/footer'

export async function findFooter(): Promise<FooterData | null> {
  const query = `where%5Btenant.slug%5D%5Bequals%5D=${TENANT}&locale=en&depth=1&limit=1`
  const res = await fetch(`${CMS_URL}/api/footers?${query}`)
  if (!res.ok) throw new Error(`CMS footer request failed: ${res.status}`)
  const data = (await res.json()) as { docs?: FooterData[] }
  return data.docs?.[0] ?? null
}
