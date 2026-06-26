import { Trash2 } from "lucide-react";
import type { FC } from "react";
import type { PairedClient } from "@/api/gen/model/pairedClient";
import { QueryState } from "@/components/query-state";
import { Section } from "@/components/section";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";

export const ClientsView: FC<{
	clients: Loadable<PairedClient[]>;
	onUnpair: (fingerprint: string) => void;
	isUnpairing: boolean;
}> = ({ clients, onUnpair, isUnpairing }) => {
	const rows = clients.data ?? [];
	return (
		<Section>
			<h1 className="text-2xl font-semibold">{m.clients_title()}</h1>
			<QueryState
				isLoading={clients.isLoading}
				error={clients.error}
				refetch={clients.refetch}
			>
				{rows.length === 0 ? (
					<Card>
						<CardContent className="p-8 text-center text-sm text-muted-foreground">
							{m.clients_empty()}
						</CardContent>
					</Card>
				) : (
					<Card>
						<CardContent className="p-0">
							<Table>
								<TableHeader>
									<TableRow>
										<TableHead>{m.clients_name()}</TableHead>
										<TableHead>{m.clients_fingerprint()}</TableHead>
										<TableHead className="w-12" />
									</TableRow>
								</TableHeader>
								<TableBody>
									{rows.map((c) => (
										<TableRow key={c.fingerprint}>
											<TableCell className="font-medium">
												{c.subject || "—"}
											</TableCell>
											<TableCell className="font-mono text-xs text-muted-foreground">
												{c.fingerprint.slice(0, 16)}…
											</TableCell>
											<TableCell>
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.action_unpair()}
													disabled={isUnpairing}
													onClick={() => onUnpair(c.fingerprint)}
												>
													<Trash2 className="size-4 text-destructive" />
												</Button>
											</TableCell>
										</TableRow>
									))}
								</TableBody>
							</Table>
						</CardContent>
					</Card>
				)}
			</QueryState>
		</Section>
	);
};
