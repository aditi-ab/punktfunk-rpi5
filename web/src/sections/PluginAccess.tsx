import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { AlertTriangle, FolderLock, RotateCcw, Trash2 } from "lucide-react";
import type { FC } from "react";
import type { PluginAccessSnapshot } from "@/api/gen/model/pluginAccessSnapshot";
import {
	getGetPluginAccessQueryKey,
	useDecidePluginAccess,
	useGetPluginAccess,
} from "@/api/gen/plugin-access/plugin-access";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages";

export type AccessDecision = "allow" | "deny" | "forget";
export type DecideAccess = (paths: string[], decision: AccessDecision) => void;

export const usePluginAccess = () => {
	const qc = useQueryClient();
	const access = useGetPluginAccess();
	const decide = useDecidePluginAccess();
	const onDecide = async (
		plugin: string,
		paths: string[],
		decision: AccessDecision,
	) => {
		try {
			for (const path of paths) {
				const snapshot = await decide.mutateAsync({
					plugin,
					data: { path, decision },
				});
				qc.setQueryData<PluginAccessSnapshot[]>(
					getGetPluginAccessQueryKey(),
					(rows = []) => [
						...rows.filter((row) => row.plugin !== plugin),
						snapshot,
					],
				);
			}
		} catch {
			toast.error(m.plugin_access_decision_failed());
		}
	};
	return { access, busy: decide.isPending, onDecide };
};

const Mode: FC<{ write: boolean }> = ({ write }) => (
	<span className="text-xs text-muted-foreground">
		{write ? m.plugin_access_read_write() : m.plugin_access_read_only()}
	</span>
);

export const PendingAccess: FC<{
	access: PluginAccessSnapshot;
	busy: boolean;
	onDecide: DecideAccess;
}> = ({ access, busy, onDecide }) => {
	const allowAll =
		access.pending.length > 1 && access.pending.every((row) => !row.write);
	return (
		<div className="mt-3 space-y-2 border-t pt-3">
			{access.pending.map((row) => (
				<div
					key={row.path}
					className={
						row.write
							? "rounded-md border border-amber-600/40 bg-amber-500/5 p-3"
							: "rounded-md border p-3"
					}
				>
					<div className="flex items-start gap-2">
						{row.write ? (
							<AlertTriangle className="mt-0.5 size-4 shrink-0 text-amber-600 dark:text-amber-500" />
						) : (
							<FolderLock className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
						)}
						<div className="min-w-0 flex-1">
							<div className="break-all font-mono text-xs">{row.path}</div>
							<Mode write={row.write} />
							{row.reason && (
								<p className="mt-1 text-xs text-muted-foreground">
									{row.reason}
								</p>
							)}
						</div>
					</div>
					<div className="mt-2 flex flex-wrap justify-end gap-2">
						<Button
							size="sm"
							variant="outline"
							disabled={busy}
							onClick={() => onDecide([row.path], "deny")}
						>
							{m.plugin_access_dont_allow()}
						</Button>
						<Button
							size="sm"
							disabled={busy}
							onClick={() => onDecide([row.path], "allow")}
						>
							{m.plugin_access_allow()}
						</Button>
					</div>
				</div>
			))}
			{allowAll && (
				<div className="flex justify-end">
					<Button
						size="sm"
						variant="outline"
						disabled={busy}
						onClick={() =>
							onDecide(
								access.pending.map((row) => row.path),
								"allow",
							)
						}
					>
						{m.plugin_access_allow_all()}
					</Button>
				</div>
			)}
		</div>
	);
};

export const RecordedAccess: FC<{
	access: PluginAccessSnapshot;
	busy: boolean;
	onDecide: DecideAccess;
}> = ({ access, busy, onDecide }) => {
	if (access.grants.length === 0 && access.denied.length === 0) return null;
	return (
		<div className="mt-3 space-y-2 border-t pt-3">
			<div className="text-xs font-medium">{m.plugin_access_title()}</div>
			{access.grants.map((grant) => (
				<div key={grant.path} className="flex items-start gap-2 text-xs">
					<FolderLock className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
					<div className="min-w-0 flex-1">
						<div className="break-all font-mono">{grant.path}</div>
						<Mode write={grant.write} />
					</div>
					<Button
						variant="ghost"
						size="icon"
						aria-label={m.common_remove()}
						disabled={busy}
						onClick={() => onDecide([grant.path], "forget")}
					>
						<Trash2 className="size-3.5 text-destructive" />
					</Button>
				</div>
			))}
			{access.denied.map((path) => (
				<div key={path} className="flex items-start gap-2 text-xs">
					<div className="min-w-0 flex-1 break-all font-mono text-muted-foreground">
						{path}
					</div>
					<Button
						size="sm"
						variant="outline"
						disabled={busy}
						onClick={() => onDecide([path], "forget")}
					>
						<RotateCcw className="size-3.5" />
						{m.plugin_access_ask_again()}
					</Button>
				</div>
			))}
		</div>
	);
};
