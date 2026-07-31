import { motion, useReducedMotion, useTime, useTransform } from "motion/react";
import { useEffect, useRef } from "react";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

// The punktfunk lens, alive. The two overlapping circles of the brand mark are
// recreated from divs and animated as if orbiting on a path whose long axis points
// INTO the screen, so depth is the dominant motion: each circle surges toward and
// away from the viewer in antiphase, passing in front of and behind the other.
//
// The 3D is faked in JS (a perspective `scale()` + a `z-index` derived from depth)
// rather than CSS `preserve-3d` — because `mix-blend-mode` (which gives the lens
// its glowing overlap) flattens a preserve-3d context in some browsers, killing
// both the scaling and the front/back swap. Honours prefers-reduced-motion.
// Size via className (e.g. `size-8`); geometry derives from the box.

const DURATION_MS = 1600;
const R_DEPTH = 0.34; // depth amplitude (fraction of box) → the size change
const PERSP = 1.05; // perspective distance (fraction of box); smaller → stronger scaling
const R_PLANE_FIXED = 0.12; // constant in-plane offset → the two never fully eclipse
const R_PLANE_SWAY = 0.05; // small in-plane breathing
const DIAG: readonly [number, number] = [-Math.SQRT1_2, Math.SQRT1_2]; // lens axis (↙ light / ↗ deep)
const LOBE_FRAC = 0.58; // circle diameter as a fraction of the box
const REST = 0; // reduced-motion: park flat (widest lens, no depth) = the brand mark

export function Spinner({
	className,
	...props
}: React.HTMLAttributes<HTMLDivElement>) {
	const reduce = useReducedMotion();
	const ref = useRef<HTMLDivElement>(null);
	const sizeRef = useRef(0);
	const time = useTime();

	useEffect(() => {
		const el = ref.current;
		if (!el) return;
		sizeRef.current = el.clientWidth;
		const ro = new ResizeObserver((entries) => {
			const w = entries[0]?.contentRect.width;
			if (w) sizeRef.current = w;
		});
		ro.observe(el);
		return () => ro.disconnect();
	}, []);

	const angleAt = (t: number) =>
		reduce ? REST : (t / DURATION_MS) * Math.PI * 2;
	const depthAt = (t: number, side: number) =>
		side * Math.sin(angleAt(t)) * R_DEPTH;

	const transformAt = (t: number, side: number) => {
		const s = sizeRef.current;
		const angle = angleAt(t);
		const z = side * Math.sin(angle) * R_DEPTH; // world depth (toward viewer = +)
		const p = PERSP / (PERSP - z); // perspective: nearer → bigger, farther → smaller
		const mag = (R_PLANE_FIXED + R_PLANE_SWAY * Math.cos(angle)) * side;
		const x = mag * DIAG[0] * p * s;
		const y = mag * DIAG[1] * p * s;
		return `translate(-50%, -50%) translate(${x}px, ${y}px) scale(${p})`;
	};

	const tLight = useTransform(time, (t) => transformAt(t, 1));
	const tDeep = useTransform(time, (t) => transformAt(t, -1));
	// z-index follows depth, so whichever circle is nearer is painted on top.
	const zLight = useTransform(time, (t) => Math.round(depthAt(t, 1) * 1000));
	const zDeep = useTransform(time, (t) => Math.round(depthAt(t, -1) * 1000));

	const lobe = (color: string): React.CSSProperties => ({
		width: `${LOBE_FRAC * 100}%`,
		height: `${LOBE_FRAC * 100}%`,
		backgroundColor: color,
		mixBlendMode: "screen",
	});

	return (
		<div
			ref={ref}
			role="status"
			aria-label={m.common_loading()}
			className={cn("relative inline-block size-6 isolate", className)}
			{...props}
		>
			<motion.div
				className="absolute left-1/2 top-1/2 rounded-full"
				style={{
					...lobe("var(--pf-brand-light)"),
					transform: tLight,
					zIndex: zLight,
				}}
			/>
			<motion.div
				className="absolute left-1/2 top-1/2 rounded-full"
				style={{ ...lobe("var(--pf-brand)"), transform: tDeep, zIndex: zDeep }}
			/>
		</div>
	);
}
