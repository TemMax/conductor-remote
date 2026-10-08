/** The agent harnesses Conductor currently offers. */
export type PlanUsageProviderId = 'claude' | 'codex' | 'cursor' | 'opencode'

export interface PlanUsageWindow {
	id: string
	label: string
	/** Percentage of this rolling allowance consumed, clamped to 0–100. */
	usedPercent: number
	/** Unix time in milliseconds, or null when the provider omits it. */
	resetsAt: number | null
	/** Provider-reported rolling-window size. Claude does not expose this directly. */
	windowDurationMins?: number | null
	/** Claude marks the bucket currently constraining requests. */
	active?: boolean
}

export interface PlanUsageBucket {
	id: string
	label: string
	windows: PlanUsageWindow[]
}

export interface ProviderPlanUsage {
	provider: PlanUsageProviderId
	label: string
	status: 'available' | 'unavailable' | 'error'
	plan: string | null
	buckets: PlanUsageBucket[]
	/** Safe, user-facing explanation. Raw CLI failures only go to the relay log. */
	message?: string
}

export interface PlanUsageSnapshot {
	providers: ProviderPlanUsage[]
	/** When these provider reads completed, as Unix time in milliseconds. */
	fetchedAt: number
}
