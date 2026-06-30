import type { FC } from "react";
import { useLocale } from "@/lib/i18n";
import { MoonlightPairingSection } from "./MoonlightPairingCard";
import { NativePairingSection } from "./NativePairingCard";
import { PairedDevicesSection } from "./PairedDevices";
import { PendingDevicesSection } from "./PendingDevices";
import { PairingView } from "./view";

// Pairing composes four independent, self-contained sub-cards. Each subsection owns its own
// queries + mutations (in its own file, next to its presentational card). The arrangement lives in
// PairingView so the live page (these containers) and the Storybook story (pure cards + mock state)
// fill the same slots — the layout is defined once and can't drift.
export const SectionPairing: FC = () => {
	useLocale();
	return (
		<PairingView
			pending={<PendingDevicesSection />}
			native={<NativePairingSection />}
			moonlight={<MoonlightPairingSection />}
			paired={<PairedDevicesSection />}
		/>
	);
};
