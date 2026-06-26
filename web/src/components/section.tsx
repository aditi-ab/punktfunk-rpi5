import { motion, useReducedMotion } from "motion/react";
import { Children, type ReactNode } from "react";
import { cn } from "@/lib/utils";

/**
 * Page content wrapper that animates in on mount — so the content fans up into
 * place every time you navigate or load a route (the route remounts, this
 * remounts). Each direct child is staggered a beat after the previous (the same
 * on-mount-delay pattern the sidebar nav uses). Honours prefers-reduced-motion.
 */
export function Section({
	children,
	className,
}: {
	children: ReactNode;
	className?: string;
}) {
	const reduce = useReducedMotion();
	return (
		<div className={cn("flex flex-col gap-6", className)}>
			{Children.map(children, (child, i) =>
				reduce ? (
					child
				) : (
					<motion.div
						initial={{ opacity: 0, y: 16 }}
						animate={{ opacity: 1, y: 0 }}
						transition={{
							delay: 0.03 + i * 0.07,
							duration: 0.42,
							ease: [0.16, 1, 0.3, 1],
						}}
					>
						{child}
					</motion.div>
				),
			)}
		</div>
	);
}
