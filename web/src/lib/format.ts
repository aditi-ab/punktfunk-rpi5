import { getLocale } from "@/paraglide/runtime";

/**
 * Date/time and number formatting that follows the CONSOLE's locale, not the browser's.
 *
 * A bare `toLocaleString()` uses `navigator.language`, so a console switched to German still
 * rendered US-style timestamps (and vice versa) — the app said one thing and its dates another.
 * `getLocale()` is Paraglide's resolved locale, which is what every string on screen uses.
 *
 * `Intl` formatters are expensive to construct and these run per table row, so they are cached
 * per locale.
 */
const dateTimeCache = new Map<string, Intl.DateTimeFormat>();

function dateTimeFor(locale: string): Intl.DateTimeFormat {
	let f = dateTimeCache.get(locale);
	if (!f) {
		f = new Intl.DateTimeFormat(locale, {
			dateStyle: "medium",
			timeStyle: "short",
		});
		dateTimeCache.set(locale, f);
	}
	return f;
}

/** Unix MILLISECONDS → a locale date-time, or an em dash for "never". */
export function fmtDateTime(unixMs: number | undefined | null): string {
	if (!unixMs) return "—";
	return dateTimeFor(getLocale()).format(new Date(unixMs));
}

/** Unix SECONDS → a locale date-time (the store's `fetched_at` convention). */
export function fmtDateTimeSecs(unixSecs: number | undefined | null): string {
	if (!unixSecs) return "—";
	return fmtDateTime(unixSecs * 1000);
}

/** A number with the console locale's separators — never a hand-rolled `toFixed`. */
export function fmtNumber(value: number, digits = 0): string {
	return new Intl.NumberFormat(getLocale(), {
		minimumFractionDigits: digits,
		maximumFractionDigits: digits,
	}).format(value);
}
