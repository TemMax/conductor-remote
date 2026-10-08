export type ToolUsageRange = '24h' | '7d' | '30d'

export interface ToolUsageRow {
	/** Null when Conductor saved a result without its matching call. */
	name: string | null
	calls: number
	inputTokens: number
	outputTokens: number
	totalTokens: number
	largestCallTokens: number
}

export interface ToolUsageProvider {
	provider: string
	sessionCount: number
	tools: ToolUsageRow[]
}

export interface ToolUsageSnapshot {
	range: ToolUsageRange
	/** UTC ISO timestamps bounding the saved traffic, not provider billing windows. */
	since: string
	until: string
	fetchedAt: number
	providers: ToolUsageProvider[]
}
