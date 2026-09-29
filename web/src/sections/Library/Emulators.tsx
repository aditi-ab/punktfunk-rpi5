import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { Cpu, Download, Trash2 } from "lucide-react";
import type { FC } from "react";
import {
	getGetEmulatorsQueryKey,
	useGetEmulators,
	useInstallEmulator,
	useRemoveEmulator,
} from "@/api/gen/emulators/emulators";
import type { EmulatorStatus } from "@/api/gen/model/emulatorStatus";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { m } from "@/paraglide/messages";

/** One line on where the emulator is, or why it is not. */
const Where: FC<{ e: EmulatorStatus }> = ({ e }) => {
	if (e.managed) {
		return (
			<span className="text-xs text-muted-foreground">
				{m.emulators_managed()}
				{e.managed.version ? ` · ${e.managed.version}` : ""}
			</span>
		);
	}
	const found = e.detected[0];
	if (found) {
		return (
			<span className="break-all font-mono text-xs text-muted-foreground">
				{found.exe}
			</span>
		);
	}
	return (
		<span className="text-xs text-muted-foreground">
			{e.offered ? m.emulators_not_installed() : m.emulators_not_offered()}
		</span>
	);
};

/**
 * Every emulator the host knows: installed by punktfunk, found on the box, or neither. Installing
 * is the operator's act here; a plugin's own ask lands in its source's Access rows instead.
 */
export const EmulatorsCard: FC = () => {
	const qc = useQueryClient();
	const rows = useGetEmulators();
	const refresh = () =>
		qc.invalidateQueries({ queryKey: getGetEmulatorsQueryKey() });
	const install = useInstallEmulator({
		mutation: {
			onSuccess: refresh,
			onError: () => toast.error(m.emulators_install_failed()),
		},
	});
	const remove = useRemoveEmulator({
		mutation: {
			onSuccess: refresh,
			onError: () => toast.error(m.emulators_remove_failed()),
		},
	});
	const busy = install.isPending || remove.isPending;
	const list = rows.data ?? [];
	if (list.length === 0) return null;
	return (
		<Card>
			<CardHeader className="pb-3">
				<CardTitle className="flex items-center gap-2">
					<Cpu className="size-4" />
					{m.emulators_title()}
				</CardTitle>
				<p className="text-sm text-muted-foreground">
					{m.emulators_description()}
				</p>
			</CardHeader>
			<CardContent className="space-y-2">
				{list.map((e) => (
					<div
						key={e.id}
						className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md border p-3"
					>
						<div className="min-w-0 flex-1">
							<div className="text-sm font-medium">{e.name}</div>
							<div className="text-xs text-muted-foreground">
								{e.platforms.join(", ")}
							</div>
							<Where e={e} />
						</div>
						<div className="flex gap-2">
							{e.managed ? (
								<Button
									size="sm"
									variant="outline"
									disabled={busy}
									onClick={() =>
										remove.mutate({ id: e.id, data: { purge: false } })
									}
								>
									<Trash2 className="size-3.5" />
									{m.emulators_remove()}
								</Button>
							) : null}
							{e.offered ? (
								<Button
									size="sm"
									variant={e.managed ? "outline" : "default"}
									disabled={busy}
									onClick={() => install.mutate({ id: e.id })}
								>
									<Download className="size-3.5" />
									{e.managed ? m.emulators_reinstall() : m.emulators_install()}
								</Button>
							) : null}
						</div>
					</div>
				))}
			</CardContent>
		</Card>
	);
};
