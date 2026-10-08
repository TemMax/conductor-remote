export interface NoSleepState {
	/** False when `nosleep setup` hasn't been run — every action here is unavailable. */
	available: boolean
	armed: boolean
	/** Epoch ms the window expires, or null for "until stopped" / not armed. */
	until: number | null
	pid: number | null
	/** True when this window also blocks the idle screen saver that locks the session. */
	preventsScreenLock: boolean
}

export interface NoSleepResult {
	ok: boolean
	error?: string
	/** Disarm only: the lid is shut, so the relay is about to `pmset sleepnow` the Mac. */
	willSleep?: boolean
	state: NoSleepState
}
