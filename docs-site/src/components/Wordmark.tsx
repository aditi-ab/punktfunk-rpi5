// The Punktfunk "funk" wordmark — vectorised from
// Export/Punktfunk_Logo-Text_No-Border_Dark.svg (the white export background is
// dropped). Inline SVG using currentColor so it recolours per theme: the
// deep-violet brand on light, the light-violet lens highlight on dark (matching
// the marketing site). Size via height (e.g. `h-4`); width follows the viewBox.
export default function Wordmark({ className = '' }: { className?: string }) {
  return (
    <svg
      role="img"
      aria-label="Punktfunk"
      xmlns="http://www.w3.org/2000/svg"
      viewBox="0 0 579 136"
      fill="currentColor"
      className={`w-auto text-[var(--pf-brand)] dark:text-[var(--pf-highlight)] ${className}`}
    >
      <title>Punktfunk</title>
      <path d="M16.782,16.051l0,102.687l31.253,0l0,-35.563l73.436,0l0,-23.555l-73.436,0l0,-19.398l77.285,0l0,-24.171l-108.537,0Z" />
      <path d="M131.785,16.051l0,47.264c0.154,16.627 0.154,16.627 0.308,20.014c0.77,15.087 2.463,21.4 7.544,26.634c7.698,8.16 20.014,10.315 59.272,10.315c23.863,0 34.178,-0.616 43.415,-2.463c11.7,-2.463 19.552,-10.623 21.246,-22.323c0.924,-7.236 1.078,-8.929 1.54,-32.176l0,-47.264l-31.253,0l0,47.264c0,2.155 -0.154,7.082 -0.308,10.623c-0.462,9.699 -1.232,12.47 -3.695,15.087c-3.387,3.695 -9.853,4.619 -31.407,4.619c-26.634,0 -32.638,-1.693 -34.332,-9.853c-0.77,-4.157 -0.77,-4.311 -1.078,-20.476l0,-47.264l-31.253,0Z" />
      <path d="M271.575,15.943l0,102.687l31.868,0l-0.77,-76.669l3.387,0l54.038,76.669l54.346,0l0,-102.687l-31.868,0l0.77,76.515l-3.233,0l-53.73,-76.515l-54.808,0Z" />
      <path d="M420.91,15.943l0,102.687l31.253,0l0,-39.258l17.089,0l46.032,39.258l47.418,0l-64.353,-52.344l59.426,-50.959l-47.88,0l-40.644,37.873l-17.089,0l0,-37.257l-31.253,0Z" />
    </svg>
  )
}
