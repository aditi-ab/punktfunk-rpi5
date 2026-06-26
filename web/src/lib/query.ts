/**
 * The slice of a React Query result a presentational view needs: just enough to
 * drive <QueryState> + render the data. A `UseQueryResult` satisfies it directly
 * (so containers pass the query through), and stories can hand-build one without
 * mocking the network.
 */
export interface Loadable<T> {
	data?: T;
	isLoading: boolean;
	error: unknown;
	refetch?: () => void;
}
