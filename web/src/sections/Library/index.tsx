import Section from "@unom/ui/section";
import { Plus } from "lucide-react";
import { type FC, useState } from "react";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import { Button } from "@/components/ui/button";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { type FormTarget, GameFormSection } from "./GameForm";
import { LibraryGridSection } from "./LibraryGrid";
import { ProvidersCard } from "./Providers";
import { SourceTogglesSection } from "./SourceToggles";

// Library = an OVERVIEW grid + a SEPARATE add/edit form, deliberately split into their own files
// (LibraryGrid / GameForm) so the two concerns never share a component. This container owns only the
// shared "is the form open, and for what" UI state; the grid and form each own their own data.
export const SectionLibrary: FC = () => {
	useLocale();
	// null = form hidden; "new" = adding; a GameEntry = editing that custom entry. Keying the form
	// by the target re-seeds its fields when switching add → edit (or between entries).
	const [target, setTarget] = useState<FormTarget | null>(null);
	// The full list, lifted from the grid so the providers card can count owners without a second
	// copy of the same query, plus which provider (if any) the grid is filtered to.
	const [entries, setEntries] = useState<GameEntry[]>([]);
	const [providerFilter, setProviderFilter] = useState<string | null>(null);

	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<div className="flex items-center justify-between gap-4">
					<h1 className="text-2xl font-semibold">{m.library_title()}</h1>
					{target === null && (
						<Button onClick={() => setTarget("new")}>
							<Plus className="size-4" />
							{m.library_add_button()}
						</Button>
					)}
				</div>

				{target !== null && (
					<GameFormSection
						key={target === "new" ? "new" : target.id}
						target={target}
						onClose={() => setTarget(null)}
					/>
				)}

				<SourceTogglesSection />

				<ProvidersCard
					entries={entries}
					active={providerFilter}
					onFilter={setProviderFilter}
				/>

				<LibraryGridSection
					onEdit={(entry) => setTarget(entry)}
					providerFilter={providerFilter}
					onEntries={setEntries}
				/>
			</div>
		</Section>
	);
};
