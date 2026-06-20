// The punktfunk "funk" wordmark — the real brand typo from the marketing site.
// The source asset is a single light-violet variant (made for dark surfaces), so
// rather than an <img> we paint it as a CSS mask and colour it per theme: the
// deep-violet brand on light, the light-violet lens highlight on dark (matching
// the site). Size it by setting a height (e.g. `h-5`); width follows the 579×136
// aspect ratio.
const maskStyle = {
  maskImage: 'url(/funk-wordmark.webp)',
  WebkitMaskImage: 'url(/funk-wordmark.webp)',
  maskRepeat: 'no-repeat',
  WebkitMaskRepeat: 'no-repeat',
  maskSize: 'contain',
  WebkitMaskSize: 'contain',
  maskPosition: 'center',
  WebkitMaskPosition: 'center',
} as const

export default function Wordmark({ className = '' }: { className?: string }) {
  return (
    <span
      role="img"
      aria-label="punktfunk"
      style={maskStyle}
      className={`inline-block aspect-[579/136] bg-[var(--pf-brand)] dark:bg-[var(--pf-highlight)] ${className}`}
    />
  )
}
