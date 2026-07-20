import { AnimatedCard } from "@unom/ui/card";
import type { ComponentProps } from "react";
import * as React from "react";
import { cn } from "@/lib/utils";

// The console's Card IS @unom/ui's animated card — a `bg-neutral` (#1c1530)
// surface with a soft brand-violet ring, on-mount motion + material gloss
// (enabled via UnomProviders). We keep the composed shadcn-style sub-component
// API (CardHeader/Title/Description/Content/Footer own their own padding), so
// the card defaults to `padding={false}` to avoid doubling it, and soften the
// 2px ring to a subtle 1px brand tint.
type CardProps = ComponentProps<typeof AnimatedCard>;

const Card = ({
	className,
	padding = false,
	children,
	...props
}: CardProps) => (
	<AnimatedCard
		padding={padding}
		className={cn("ring-1 ring-accent/40", className)}
		{...props}
	>
		{children}
	</AnimatedCard>
);
Card.displayName = "Card";

const CardHeader = React.forwardRef<
	HTMLDivElement,
	React.HTMLAttributes<HTMLDivElement>
>(({ className, ...props }, ref) => (
	<div
		ref={ref}
		className={cn("flex flex-col space-y-1.5 p-4 sm:p-6", className)}
		{...props}
	/>
));
CardHeader.displayName = "CardHeader";

const CardTitle = React.forwardRef<
	HTMLDivElement,
	React.HTMLAttributes<HTMLDivElement>
>(({ className, ...props }, ref) => (
	<div
		ref={ref}
		className={cn("font-semibold leading-none tracking-tight", className)}
		{...props}
	/>
));
CardTitle.displayName = "CardTitle";

const CardDescription = React.forwardRef<
	HTMLDivElement,
	React.HTMLAttributes<HTMLDivElement>
>(({ className, ...props }, ref) => (
	<div
		ref={ref}
		className={cn("text-sm text-muted-foreground", className)}
		{...props}
	/>
));
CardDescription.displayName = "CardDescription";

/**
 * Card body. Pass `flush` for content that should meet the card's edges — a full-bleed table, most
 * commonly — instead of trying to cancel the padding from the outside.
 *
 * `className="p-0"` does NOT work for that: tailwind-merge only resolves conflicts *within the same
 * variant*, so `p-0` cancels `p-4` but leaves `sm:p-6` standing, and the padding silently returns at
 * ≥640px. Every call site that tried it ended up with a doubled inset once a `CardHeader` (which
 * brings its own `sm:p-6`) was nested inside — visible as one card whose title sits 24px further in
 * than its neighbours'.
 */
const CardContent = React.forwardRef<
	HTMLDivElement,
	React.HTMLAttributes<HTMLDivElement> & { flush?: boolean }
>(({ className, flush = false, ...props }, ref) => (
	<div
		ref={ref}
		className={cn(!flush && "p-4 pt-0 sm:p-6 sm:pt-0", className)}
		{...props}
	/>
));
CardContent.displayName = "CardContent";

const CardFooter = React.forwardRef<
	HTMLDivElement,
	React.HTMLAttributes<HTMLDivElement>
>(({ className, ...props }, ref) => (
	<div
		ref={ref}
		className={cn("flex items-center p-4 pt-0 sm:p-6 sm:pt-0", className)}
		{...props}
	/>
));
CardFooter.displayName = "CardFooter";

export {
	Card,
	CardContent,
	CardDescription,
	CardFooter,
	CardHeader,
	CardTitle,
};
