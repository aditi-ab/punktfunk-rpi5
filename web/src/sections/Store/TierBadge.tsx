import {
	BadgeCheck,
	ShieldAlert,
	ShieldQuestion,
	Terminal,
} from "lucide-react";
import type { FC } from "react";
import type { StoreTier } from "@/api/store";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

// The trust model, rendered. These badges are the only place the console makes a claim about where
// a plugin's code came from, so they are deliberately literal: ONLY the built-in `unom` source earns
// the check mark, everything else says out loud that nobody at unom looked at the code. An entry
// from an operator-added source additionally carries a <SourceChip> naming that source — attribution
// instead of a verification it hasn't got.

/** The short, permanent provenance badge for a tier. */
export const TierBadge: FC<{ tier: StoreTier; className?: string }> = ({
	tier,
	className,
}) => {
	switch (tier) {
		case "verified":
			return (
				<Badge
					variant="success"
					className={cn("gap-1", className)}
					title={m.store_tier_verified_hint()}
				>
					<BadgeCheck className="size-3.5" />
					{m.store_tier_verified()}
				</Badge>
			);
		case "external":
			return (
				<Badge
					variant="outline"
					className={cn(
						"gap-1 border-amber-600/40 text-amber-600 dark:border-amber-500/40 dark:text-amber-500",
						className,
					)}
					title={m.store_tier_external_hint()}
				>
					<ShieldQuestion className="size-3.5" />
					{m.store_tier_external()}
				</Badge>
			);
		case "unverified":
			return (
				<Badge
					variant="destructive"
					className={cn("gap-1", className)}
					title={m.store_tier_unverified_hint()}
				>
					<ShieldAlert className="size-3.5" />
					{m.store_tier_unverified()}
				</Badge>
			);
		default:
			return (
				<Badge
					variant="secondary"
					className={cn("gap-1", className)}
					title={m.store_tier_cli_hint()}
				>
					<Terminal className="size-3.5" />
					{m.store_tier_cli()}
				</Badge>
			);
	}
};

/** "from <source>" — who curated this entry. Shown for anything not from the built-in source. */
export const SourceChip: FC<{ source: string; className?: string }> = ({
	source,
	className,
}) => (
	<span
		className={cn(
			"inline-flex items-center gap-1 text-xs text-muted-foreground",
			className,
		)}
	>
		{m.store_from_source()}
		<span className="font-medium text-foreground">{source}</span>
	</span>
);
