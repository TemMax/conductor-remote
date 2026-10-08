import type { ParkedAgentPatch } from './parked.ts'

export type FirstPromptStatus = 'waiting' | 'failed'

export interface FirstPrompt {
	autoModel?: true
	workspaceId: string
	text: string
	/** Agent choices selected while the workspace was being created. */
	agent?: ParkedAgentPatch
	/** Legacy persisted model choice, migrated into `agent` when delivery resumes. */
	model?: string
	/** Files staged before this workspace had a worktree. They must land before `text` is sent. */
	attachmentIds?: string[]
	status: FirstPromptStatus
	/** Sends already spent on it *after* the worktree turned ready — the budget that counts. */
	attempts: number
	/** Sends tried before that, bounded separately and never fatal. */
	earlyAttempts?: number
	/**
	 * Try the send before the worktree is built, which is what makes a prompt land in
	 * seconds instead of minutes (see the header). Default on; `false` restores the
	 * old wait for a repo whose setup script the agent's first move depends on, and
	 * an entry written before this existed has no field and gets the default.
	 */
	sendImmediately?: boolean
	/** When the last send finished, so the next is spaced without sleeping the loop (see `step`). */
	lastAttemptAt?: number
	createdAt: number
	/** Why it was given up on — shown on the phone beside the undelivered text. */
	error?: string
}
