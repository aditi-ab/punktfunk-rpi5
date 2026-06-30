import Section from "@unom/ui/section";
import type { FC, ReactNode } from "react";
import { m } from "@/paraglide/messages";

/**
 * The Pairing page LAYOUT — the single source of how the four sub-cards are arranged. Both the live
 * page (`index.tsx`, slots = the self-contained `*Section` containers) and Storybook (slots = the
 * pure cards with mock state) fill these slots, so the arrangement can never drift between them.
 */
export const PairingView: FC<{
	pending: ReactNode;
	native: ReactNode;
	moonlight: ReactNode;
	paired: ReactNode;
}> = ({ pending, native, moonlight, paired }) => (
	<Section maxWidth={false}>
		<div className="flex flex-col gap-card">
			<h1 className="text-2xl font-semibold">{m.pairing_title()}</h1>

			{pending}
			<div className="lg:grid lg:grid-cols-2 flex flex-col gap-card">
				{native}
				{moonlight}
			</div>
			{paired}
		</div>
	</Section>
);
