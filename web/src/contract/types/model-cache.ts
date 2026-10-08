export interface CachedModelGroup {
	/** Harness of the chat where this whole picker menu was observed; not ownership of every row. */
	agentType: string
	models: string[]
	/** The user-wide picker row Conductor last showed with its star selected. */
	defaultModel?: string
	/**
	 * Time of the complete picker observation backing this entry. `null` means
	 * the entry contains only models learned from successful selections. Older
	 * cache files omit this field; their provenance is unknown, so they cannot
	 * establish that other model labels have disappeared.
	 */
	snapshotAt?: number | null
	/** The actual observed menu, kept separate from labels appended by later selections. */
	snapshotModels?: string[]
	/** Positive evidence tracked per label so selecting another row cannot refresh a stale one. */
	selections?: Array<{ model: string; selectedAt: number }>
	updatedAt: number
}
