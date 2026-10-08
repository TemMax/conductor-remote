export interface ContextCategories {
	/** System/developer prompts, instructions, tool definitions, attachments, summaries, and protocol overhead. */
	initial: number
	/** User prompts plus visible assistant prose. */
	chat: number
	/** Provider reasoning/thinking blocks, excluding opaque signatures. */
	thinking: number
	/** Tool calls and their results. */
	tools: number
}

export interface ContextBreakdown {
	/** The provider-owned total Conductor persisted for the last completed turn. */
	totalTokens: number
	usedPercent: number | null
	/** Whether the active window follows at least one compaction boundary. */
	compacted: boolean
	categories: ContextCategories
	/** Approximate attachment sizes for the fork choices that copy the whole chat. */
	forkTokens: {
		concise: number
		reasoning: number
		full: number
	}
}
