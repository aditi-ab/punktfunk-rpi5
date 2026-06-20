// The docs reuse the punktfunk marketing site's footer — the same Payload CMS
// global on the shared unom CMS (cms.unom.io). It's a read-only GET, so a plain
// typed fetch rather than pulling in the Payload SDK + generated types.
const CMS_URL = 'https://cms.unom.io'

export interface NavigationLink {
  id?: string | null
  label?: string | null
  to?: string | null
}

export interface NavigationSection {
  id?: string | null
  title?: string | null
  entries?: NavigationLink[] | null
}

export interface Footer {
  tagline?: string | null
  sections?: NavigationSection[] | null
}

export async function findFooter(): Promise<Footer> {
  const res = await fetch(`${CMS_URL}/api/globals/footer?locale=en&depth=1`)
  if (!res.ok) throw new Error(`CMS footer request failed: ${res.status}`)
  return res.json() as Promise<Footer>
}
