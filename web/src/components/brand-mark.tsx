// punktfunk brand mark: two overlapping circles forming a lens — the violet
// brand identity (flattened from the clients/apple punktfunk_Logo.icon, shared
// verbatim with the marketing site + docs). Back-to-front: large light-violet
// circle, deep-violet circle, light highlight where they overlap.
export function BrandMark({ className }: { className?: string }) {
	return (
		<svg
			aria-label="punktfunk"
			role="img"
			xmlns="http://www.w3.org/2000/svg"
			viewBox="0 0 1000 1000"
			className={className}
		>
			<title>punktfunk</title>
			<path
				d="M403.037,791.672c107.586,0 194.41,-86.824 194.41,-194.41c0,-107.586 -86.824,-194.41 -194.41,-194.41c-107.586,0 -194.41,86.824 -194.41,194.41c0,107.586 86.824,194.41 194.41,194.41Z"
				fill="#a79ff8"
			/>
			<path
				d="M735.276,540.321c76.075,-76.075 76.075,-198.862 0,-274.937c-76.075,-76.075 -198.862,-76.075 -274.937,0c-76.075,76.075 -76.075,198.862 0,274.937c76.075,76.075 198.862,76.075 274.937,0Z"
				fill="#6c5bf3"
			/>
			<path
				d="M647.84,590.737c-64.853,17.403 -136.871,0.597 -187.885,-50.416c-51.013,-51.013 -67.819,-123.032 -50.416,-187.885c64.853,-17.403 136.871,-0.597 187.885,50.416c51.013,51.013 67.819,123.032 50.416,187.885Z"
				fill="#d2c9fb"
			/>
		</svg>
	);
}

export default BrandMark;
