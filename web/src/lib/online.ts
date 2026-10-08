/** Keep a brief network handoff from showing as a relay outage. */
export const OFFLINE_GRACE_MS = 10_000

/** Milliseconds left before a failed request may show the offline state. */
export function offlineDelay(lastSyncAt: number | null, now = Date.now()): number {
	return Math.max(0, (lastSyncAt ?? now) + OFFLINE_GRACE_MS - now)
}
