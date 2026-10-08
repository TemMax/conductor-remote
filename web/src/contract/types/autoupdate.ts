export type UpdateMode = 'off' | 'check' | 'auto'

export interface UpdateStatus {
	/** Version this process is running (from the package's own package.json). */
	current: string
	/** Latest version seen on the registry, or null before the first successful check. */
	latest: string | null
	/** True when `latest` is a strictly higher release than `current`. */
	available: boolean
	/** Epoch ms of the last successful registry check, or null. */
	checkedAt: number | null
	/** Effective mode after the gates are applied. */
	mode: UpdateMode
	/** Last check/install error message, or null. */
	lastError: string | null
}
