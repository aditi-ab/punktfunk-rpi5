import { useEffect, useState } from "react";

/** `value`, once it has held still for `ms`. A search box asks the host when typing pauses. */
export function useDebounced<T>(value: T, ms: number): T {
	const [held, setHeld] = useState(value);
	useEffect(() => {
		const t = setTimeout(() => setHeld(value), ms);
		return () => clearTimeout(t);
	}, [value, ms]);
	return held;
}
