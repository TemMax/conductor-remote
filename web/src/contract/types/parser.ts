export interface QuestionRequest {
	id: string
	provider: 'claude' | 'codex'
	questions: {
		header: string | null
		question: string
		options: { label: string; description: string }[]
		multiSelect: boolean
	}[]
}

export interface QuestionAnswer {
	selected: number[]
	other?: string | null
}

export interface TranscriptEntry {
	question?: QuestionRequest
	id: string
	rowid: number
	/** Display role: user | assistant | tool | thinking | system */
	role: 'user' | 'assistant' | 'tool' | 'thinking' | 'system'
	/** Human-readable text. For tool rows: the call's description, else the tool name. */
	text: string
	/** Tool name when role === 'tool'. */
	tool?: string
	/** Full mono secondary detail for tool rows (command, path, pattern, …). */
	detail?: string
	/**
	 * The SDK's `tool_use` id. Carried by the call and by the result answering it — the
	 * only thing that pairs the two, which sit in different `session_messages` rows.
	 */
	toolUseId?: string
	/**
	 * The agent tool call whose transcript this frame belongs to.
	 *
	 * Conductor writes subagent frames into the parent chat and points every one
	 * back at the spawning `tool_use`. The phone uses this durable join to nest the work
	 * under that call even when the parent keeps speaking while the subagent runs.
	 */
	parentToolUseId?: string
	/** Human label for a tool call that spawned a subagent. Present only on that call. */
	subagentLabel?: string
	/** A tool result's output, clipped. The phone folds it onto the call row. */
	output?: string
	/** True when `output` is a unified diff (an edit's result), so the phone colours it. */
	diff?: boolean
	/** Images the result carried, as `GET /api/tool-images/:reference` references. */
	images?: string[]
	/** True when this row is a failed tool result. */
	error?: boolean
	ts: string
	/** True while the message is queued, from the current outbox or the legacy in-row signal. */
	queued: boolean
}
