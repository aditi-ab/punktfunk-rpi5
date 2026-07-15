import type { FC } from "react";
import { useGetHostInfo } from "@/api/gen/host/host";
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
	// Moonlight/GameStream pairing only works when the host runs the compat planes (`--gamestream`,
	// off by default). Otherwise a Moonlight PIN can never arrive, so the card is dead UI — hide it
	// (and until host info loads, to avoid a flash of an un-actionable card).
	const host = useGetHostInfo();
	const gamestream = host.data?.gamestream === true;
	return (
		<PairingView
			pending={<PendingDevicesSection />}
			native={<NativePairingSection />}
			moonlight={gamestream ? <MoonlightPairingSection /> : null}
			paired={<PairedDevicesSection />}
		/>
	);
};
