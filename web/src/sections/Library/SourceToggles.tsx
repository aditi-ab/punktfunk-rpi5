import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { Check } from "lucide-react";
import type { FC } from "react";
import {
	getGetLibraryQueryKey,
	getListLibraryScannersQueryKey,
	useListLibraryScanners,
	useSetLibraryScanner,
} from "@/api/gen/library/library";
import type { ScannerInfo } from "@/api/gen/model/scannerInfo";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { m } from "@/paraglide/messages";

/**
 * Container: the game-source (library scanner) toggles — owns the scanner query and the toggle
 * mutation. The host only reports the scanners its platform actually has (Steam everywhere,
 * Lutris/Heroic on Linux, Epic/GOG/Xbox on Windows), so whatever arrives is renderable as-is.
 * Rendered only once the list is loaded: this is a secondary control, and when the API is down
 * the grid's own QueryState already tells the story — no second error banner.
 */
export const SourceTogglesSection: FC = () => {
	const qc = useQueryClient();
	const scanners = useListLibraryScanners();
	const toggle = useSetLibraryScanner();

	const onToggle = async (scanner: ScannerInfo) => {
		try {
			// The PUT answers with the full updated list — seed the query cache with it directly,
			// then refetch the library so the grid reflects the new source set.
			const list = await toggle.mutateAsync({
				id: scanner.id,
				data: { enabled: !scanner.enabled },
			});
			qc.setQueryData(getListLibraryScannersQueryKey(), list);
			await qc.invalidateQueries({ queryKey: getGetLibraryQueryKey() });
		} catch {
			toast.error(m.library_sources_failed());
		}
	};

	if (!scanners.data) return null;
	return (
		<SourceToggles
			scanners={scanners.data}
			busyId={toggle.isPending ? (toggle.variables?.id ?? null) : null}
			onToggle={onToggle}
		/>
	);
};

/** The sources card: one pressed/unpressed chip per scanner (pressed = the host scans it). */
export const SourceToggles: FC<{
	scanners: ScannerInfo[];
	/** Scanner id whose toggle is in flight, or null — only that chip disables. */
	busyId: string | null;
	onToggle: (scanner: ScannerInfo) => void;
}> = ({ scanners, busyId, onToggle }) => (
	<Card>
		<CardHeader className="pb-3">
			<CardTitle className="text-base">{m.library_sources_title()}</CardTitle>
		</CardHeader>
		<CardContent className="space-y-3">
			<div className="flex flex-wrap gap-2">
				{scanners.map((scanner) => (
					<Button
						key={scanner.id}
						size="sm"
						variant={scanner.enabled ? "default" : "outline"}
						aria-pressed={scanner.enabled}
						disabled={busyId === scanner.id}
						onClick={() => onToggle(scanner)}
					>
						{scanner.enabled && <Check className="size-4" />}
						{scanner.label}
					</Button>
				))}
			</div>
			<p className="max-w-prose text-xs text-muted-foreground">
				{m.library_sources_help()}
			</p>
		</CardContent>
	</Card>
);
