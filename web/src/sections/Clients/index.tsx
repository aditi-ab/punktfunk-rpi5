import { useQueryClient } from "@tanstack/react-query";
import type { FC } from "react";
import {
	getListPairedClientsQueryKey,
	useListPairedClients,
	useUnpairClient,
} from "@/api/gen/clients/clients";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { ClientsView } from "./view";

export const SectionClients: FC = () => {
	useLocale();
	const qc = useQueryClient();
	const clients = useListPairedClients();
	const unpair = useUnpairClient();

	const onUnpair = (fingerprint: string) => {
		if (!confirm(m.clients_unpair_confirm())) return;
		unpair.mutate(
			{ fingerprint },
			{
				onSuccess: () =>
					qc.invalidateQueries({ queryKey: getListPairedClientsQueryKey() }),
			},
		);
	};

	return (
		<ClientsView
			clients={clients}
			onUnpair={onUnpair}
			isUnpairing={unpair.isPending}
		/>
	);
};
