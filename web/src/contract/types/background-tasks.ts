/** One background task a chat is still waiting on. */
export interface BackgroundTask {
	taskId: string
	/** The `tool_use` that started it — the Bash or Agent row the phone already shows. */
	toolUseId: string | null
	/** The call's own description, which is what the desktop prints beside "Waiting for task". */
	description: string
	/** `local_bash` for a background command, `local_agent` for a background subagent. */
	taskType: string
	/** When the task started, ISO — the elapsed timer's origin. */
	since: string
}
