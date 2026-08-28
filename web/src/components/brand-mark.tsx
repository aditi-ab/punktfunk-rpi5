// Punktfunk brand mark: two overlapping circles forming a lens — the violet
// brand identity (flattened from the clients/apple punktfunk_Logo.icon, shared
// verbatim with the marketing site + docs). Back-to-front: large light-violet
// circle, deep-violet circle, light highlight where they overlap.
//
// The three fills are the brand TOKENS, not the violet literals they default to, so
// the mark re-tints with the rest of the console on an Omarchy box that gave us a
// theme (see the `[data-omarchy]` block in styles.css). Each keeps its literal as a
// var() fallback, so the mark is still correct anywhere the stylesheet is not.
// Inline `style` rather than a `fill=` attribute: var() in a presentation attribute
// is not reliable across engines, and in a style declaration it always is.
export function BrandMark({ className }: { className?: string }) {
	return (
		<svg
			aria-label="Punktfunk"
			role="img"
			xmlns="http://www.w3.org/2000/svg"
			viewBox="0 0 1000 1000"
			className={className}
		>
			<title>Punktfunk</title>
			<path
				d="M403.037,791.672c107.586,0 194.41,-86.824 194.41,-194.41c0,-107.586 -86.824,-194.41 -194.41,-194.41c-107.586,0 -194.41,86.824 -194.41,194.41c0,107.586 86.824,194.41 194.41,194.41Z"
				style={{ fill: "var(--pf-brand-light, #a79ff8)" }}
			/>
			<path
				d="M735.276,540.321c76.075,-76.075 76.075,-198.862 0,-274.937c-76.075,-76.075 -198.862,-76.075 -274.937,0c-76.075,76.075 -76.075,198.862 0,274.937c76.075,76.075 198.862,76.075 274.937,0Z"
				style={{ fill: "var(--pf-brand, #6c5bf3)" }}
			/>
			<path
				d="M647.84,590.737c-64.853,17.403 -136.871,0.597 -187.885,-50.416c-51.013,-51.013 -67.819,-123.032 -50.416,-187.885c64.853,-17.403 136.871,-0.597 187.885,50.416c51.013,51.013 67.819,123.032 50.416,187.885Z"
				style={{ fill: "var(--pf-highlight, #d2c9fb)" }}
			/>
		</svg>
	);
}

export default BrandMark;
