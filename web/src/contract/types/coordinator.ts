/**
 * Which kind of prose matched. `thinking` is separate from `assistant` on purpose:
 * a hit there is reasoning the agent never said out loud, and labelling it as the
 * agent's answer would misread it. The three are exactly `TranscriptEntry['role']`
 * minus the parts this index skips (`tool`, `system`).
 */
export type SearchRole = 'user' | 'assistant' | 'thinking'

export interface IndexStatus {
	/** Chunks indexed so far. */
	chunks: number
	/** True once the backfill has reached the newest message. */
	ready: boolean
	/** 0–1 through the source rows; 1 when caught up. */
	progress: number
	/** Present when the sidecar DB could not be opened at all. */
	error?: string
}

/** One matching excerpt, as the phone renders it. */
export interface SearchSnippet {
	sessionId: string
	/** Opaque source-message pointer for a bounded `read_chat` around this hit. */
	cursor: string
	role: SearchRole
	at: string
	/** Hits wrapped in HIT_OPEN/HIT_CLOSE. */
	text: string
}

/** A workspace a search matched, with the evidence. */
export interface SearchResult<W> {
	workspace: W
	/** The chat holding this workspace's strongest passage — where a tap should land. */
	sessionId: string | null
	/** Number of matching messages, all of them. */
	hits: number
	/** Higher is better. The summed score of the snippets below, and only those. */
	score: number
	/** Most recent matching message. */
	at: string | null
	snippets: SearchSnippet[]
	/** True when the workspace's own name/branch matched, rather than (only) its chats. */
	byName: boolean
}
