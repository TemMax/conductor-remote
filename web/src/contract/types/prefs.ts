import type { AgentDraft } from './agent-inputs.ts'

/** A ready file reference carried with one unsent composer draft. The bytes stay on the host. */
export interface DraftAttachment {
	name: string
	path: string
	bytes: number
	token: string
	/** Fork context stays outside the composer and travels with the next user message. */
	source?: 'fork'
	/** Present only while a New Workspace file is waiting outside its future worktree. */
	stageId?: string
}

export interface SyncedDraft {
	/** Empty is valid when agent settings have been staged before any text is typed. */
	text: string
	/** Text and its next-send agent choices are one intent and therefore one revision. */
	agent: AgentDraft
	/** Ready attachments are the same intent; uploads still in flight never leave their source device. */
	attachments: DraftAttachment[]
	/** Client-side logical timestamp. Newer revisions win; deletion wins an exact tie. */
	updatedAt: number
	/** Kept as a tombstone so an offline device cannot restore an already-sent draft. */
	deleted: boolean
}

export interface Prefs {
	/** Session `updated_at` values. A mark can only advance, so these merge by max. */
	readMarks: Record<string, string>
	drafts: Record<string, SyncedDraft>
}
