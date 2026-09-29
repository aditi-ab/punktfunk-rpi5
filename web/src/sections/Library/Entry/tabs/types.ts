import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import type { FormState } from "../model";

export interface TabProps {
	draft: FormState;
	set: <K extends keyof FormState>(key: K, value: FormState[K]) => void;
	readOnly: boolean;
	/** The catalog entry; null while creating. */
	entry: OperatorGameEntry | null;
	/** The draft as last saved, to tell an edited field from a stored one. */
	baseline: FormState;
}
