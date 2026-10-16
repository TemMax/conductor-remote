import type { ArtifactPreviewDiagnostic, ArtifactPreviewMessage, ArtifactPreviewSession } from '../../lib/types.ts'

export const MAX_DIAGNOSTICS = 100
export const MAX_MESSAGE = 2000
export const MAX_SOURCE = 1000
export const ARTIFACT_SANDBOX = 'allow-scripts allow-same-origin'

/** Reject a malformed server response before putting any URL into an iframe. */
export function isolatedSession(session: ArtifactPreviewSession, parentOrigin: string): boolean {
	try {
		const origin = new URL(session.origin)
		const bootstrap = new URL(session.bootstrapUrl)
		return (
			['http:', 'https:'].includes(origin.protocol) &&
			origin.origin === session.origin &&
			origin.origin !== parentOrigin &&
			!origin.username &&
			!origin.password &&
			bootstrap.origin === origin.origin &&
			!bootstrap.username &&
			!bootstrap.password &&
			bootstrap.pathname === '/__conductor_preview/bootstrap' &&
			!!session.id &&
			!!session.grant &&
			Number.isFinite(Date.parse(session.expiresAt))
		)
	} catch {
		return false
	}
}

type DisplayMessage = Exclude<ArtifactPreviewMessage, { type: 'authorize' }>

/** The frame may supply display data only. No message can invoke a relay operation. */
export function artifactMessage(
	event: Pick<MessageEvent, 'origin' | 'source' | 'data'>,
	session: Pick<ArtifactPreviewSession, 'id' | 'origin'>,
	frameWindow: MessageEventSource | null
): DisplayMessage | null {
	if (!frameWindow || event.source !== frameWindow || event.origin !== session.origin) return null
	const data: unknown = event.data
	if (!data || typeof data !== 'object') return null
	const value = data as Record<string, unknown>
	if (value.channel !== 'conductor-artifact-preview' || value.version !== 1 || value.sessionId !== session.id)
		return null
	const envelope = { channel: 'conductor-artifact-preview', version: 1, sessionId: session.id } as const
	if (value.type === 'ready' || value.type === 'loaded') return { ...envelope, type: value.type }
	if (
		value.type !== 'diagnostic' ||
		typeof value.kind !== 'string' ||
		!['javascript', 'promise', 'resource', 'http'].includes(value.kind)
	)
		return null
	if (typeof value.message !== 'string' || (value.source !== undefined && typeof value.source !== 'string')) return null
	for (const key of ['line', 'column']) {
		if (value[key] !== undefined && (!Number.isSafeInteger(value[key]) || (value[key] as number) < 0)) return null
	}
	return {
		...envelope,
		type: 'diagnostic',
		kind: value.kind as ArtifactPreviewDiagnostic['kind'],
		message: value.message.slice(0, MAX_MESSAGE),
		...(typeof value.source === 'string' ? { source: value.source.slice(0, MAX_SOURCE) } : {}),
		...(typeof value.line === 'number' ? { line: value.line } : {}),
		...(typeof value.column === 'number' ? { column: value.column } : {})
	}
}

export function appendDiagnostic<T extends ArtifactPreviewDiagnostic>(current: T[], next: T) {
	return current.length < MAX_DIAGNOSTICS ? [...current, next] : current
}

/** One bootstrap grant per session; incoming authorize/commands never reach this state machine. */
export function artifactHandshake() {
	let authorized = false
	return (message: DisplayMessage): 'authorize' | 'loaded' | 'diagnostic' | null => {
		if (message.type === 'ready' && !authorized) {
			authorized = true
			return 'authorize'
		}
		if (!authorized) return null
		return message.type === 'loaded' || message.type === 'diagnostic' ? message.type : null
	}
}
