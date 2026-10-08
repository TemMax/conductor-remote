export type LogLevel = 'info' | 'warn' | 'error'

export interface LogEntry {
	/** Epoch ms. Null for on-disk lines with no stamp: continuation lines, or lines from before this shipped. */
	t: number | null
	level: LogLevel
	text: string
}

export interface LogFileInfo {
	name: string
	size: number
	modifiedAt: number | null
}
