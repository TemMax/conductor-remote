export interface SendResult {
	ok: boolean
	strategy: string
	warning?: string
	error?: string
}

/** How `/api/state` describes the write strategy in force (see `describeActuator`). */
export interface ActuatorInfo {
	name: string
	/** Human-readable note about this strategy's limits, surfaced in the UI. */
	caveat: string
	/** True when delivery is addressed to a specific session (no window-focus dependency). */
	precise: boolean
	/** False when the strategy's runtime check says it can't deliver right now. */
	available: boolean
}
