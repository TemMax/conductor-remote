/** The safe half of a `Device` — what `GET /api/push` may hand back over the wire. */
export interface DeviceInfo {
	/** Stable per-endpoint id (hash), so a device can be named without exposing its push URL. */
	id: string
	label: string
	createdAt: number
	lastOkAt: number | null
	lastError: string | null
	failures: number
}
