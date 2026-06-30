import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";
import { m } from "@/paraglide/messages";

/** shadcn/ui's class combiner: merge conditional classes, dedupe Tailwind conflicts. */
export function cn(...inputs: ClassValue[]) {
	return twMerge(clsx(inputs));
}

/** Seconds since a knock → a short relative label. */
export function fmtAge(secs: number): string {
	if (secs < 10) return m.pairing_pending_age_just_now();
	if (secs < 60) return m.pairing_pending_age_secs({ s: Math.floor(secs) });
	return m.pairing_pending_age_mins({ min: Math.floor(secs / 60) });
}
