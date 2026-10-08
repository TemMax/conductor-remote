import type { AgentPatch } from './agent-inputs.ts'

export type ParkedStatus = 'waiting' | 'failed'

/** Staged agent settings riding with the prompt (mirrors the phone's `AgentPatch`). */
export type ParkedAgentPatch = AgentPatch

export interface ParkedPrompt {
	autoModel?: true
	workspaceId: string
	sessionId: string
	text: string
	/** Applied before the prompt on delivery, exactly as the phone would have. */
	agent?: ParkedAgentPatch
	/** Queue behind the current turn when the Mac unlocks. */
	queue?: boolean
	status: ParkedStatus
	/** Real delivery failures with the Mac unlocked — lock re-checks don't count. */
	attempts: number
	createdAt: number
	/** What the entry is waiting for, in the words the chat shows under the bubble. */
	reason: string
	/** Why it was given up on — shown beside the undelivered text. */
	error?: string
}
