import { useQueryClient } from "@tanstack/react-query";
import type { FC } from "react";
import {
	getGetLibraryQueryKey,
	useCreateCustomGame,
	useDeleteCustomGame,
	useGetLibrary,
	useUpdateCustomGame,
} from "@/api/gen/library/library";
import type { CustomInput } from "@/api/gen/model/customInput";
import { useLocale } from "@/lib/i18n";
import { LibraryView } from "./view";

export const SectionLibrary: FC = () => {
	useLocale();
	const qc = useQueryClient();
	const library = useGetLibrary();
	const create = useCreateCustomGame();
	const update = useUpdateCustomGame();
	const remove = useDeleteCustomGame();

	const invalidate = () =>
		qc.invalidateQueries({ queryKey: getGetLibraryQueryKey() });

	return (
		<LibraryView
			library={library}
			onCreate={(data: CustomInput) =>
				create.mutateAsync({ data }).then(invalidate)
			}
			onUpdate={(id, data) => update.mutateAsync({ id, data }).then(invalidate)}
			onDelete={(id) => remove.mutateAsync({ id }).then(invalidate)}
			isSaving={create.isPending || update.isPending}
			isDeleting={remove.isPending}
		/>
	);
};
