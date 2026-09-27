// Password gate for library writes that carry `prep` or a privileged launch kind, which the host
// runs as its own user. The BFF forwards these with the admin bearer, so this is the only password
// check on that path: a 7-day session cookie alone must not leave a command behind.
//
// An ordinary edit (title, artwork, a `steam_appid` launch) runs no code and is not gated;
// prompting for it trains the operator to type the password without reading.
import type { H3Event } from "h3";
import {
	carriesCommandExecution,
	type EntryLike,
} from "../../src/lib/command-execution";
import { confirmPassword } from "./confirm";

/**
 * Re-verify the console password iff `entries` contains a command-execution field. Throws the same
 * 401/429/503 `confirmPassword` does; resolves when the gate does not apply.
 *
 * Async because the verify is: the caller must `await` this, or a gated write runs unchecked.
 */
export async function confirmIfCommandExecution(
	event: H3Event,
	entries: EntryLike | EntryLike[] | null | undefined,
	password: unknown,
): Promise<void> {
	const list = Array.isArray(entries) ? entries : [entries];
	if (list.some(carriesCommandExecution))
		await confirmPassword(event, password);
}
