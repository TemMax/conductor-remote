import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { type ArtifactTarget, startArtifact } from '../components/artifacts/lifecycle.ts'
import { appendDiagnostic, artifactHandshake, artifactMessage } from '../components/artifacts/protocol.ts'
import { client } from '../lib/api.ts'
import type { ArtifactPreviewDiagnostic, ArtifactPreviewSession, ArtifactPreviewSource } from '../lib/types.ts'

export function useArtifactPreview(target: ArtifactTarget) {
	const frame = useRef<HTMLIFrameElement>(null)
	const [revision, setRevision] = useState(0)
	const [session, setSession] = useState<ArtifactPreviewSession | null>(null)
	const [source, setSource] = useState<ArtifactPreviewSource | null>(null)
	const [error, setError] = useState<string | null>(null)
	const [sourceError, setSourceError] = useState<string | null>(null)
	const [phase, setPhase] = useState<'creating' | 'bootstrap' | 'authorizing' | 'loaded' | 'unavailable'>('creating')
	const [diagnostics, setDiagnostics] = useState<(ArtifactPreviewDiagnostic & { id: number })[]>([])
	const kind = target.kind
	const reference = target.kind === 'file' ? target.reference : ''
	const html = target.kind === 'html' ? target.html : ''
	const baseDirectory = target.kind === 'html' ? target.baseDirectory : null

	// biome-ignore lint/correctness/useExhaustiveDependencies: Refresh explicitly replaces the owned session.
	useEffect(() => {
		setSession(null)
		setSource(kind === 'html' ? { content: html, path: null } : null)
		setError(null)
		setSourceError(null)
		setDiagnostics([])
		setPhase('creating')
		let expiry: number | undefined
		const lifecycle = startArtifact(
			client,
			kind === 'file' ? { kind, reference } : { kind, html, baseDirectory },
			window.location.origin,
			value => {
				setSession(value)
				setPhase('bootstrap')
				expiry = window.setTimeout(
					() => {
						void lifecycle.dispose()
						setSession(null)
						setError('This preview session has expired. Refresh to reopen it.')
					},
					Math.max(0, Math.min(Date.parse(value.expiresAt) - Date.now(), 2_147_483_647))
				)
			},
			setSource,
			setError,
			setSourceError
		)
		const leavePage = () => {
			window.clearTimeout(expiry)
			void lifecycle.dispose()
			setSession(null)
		}
		const restorePage = (event: PageTransitionEvent) => {
			if (event.persisted) setRevision(value => value + 1)
		}
		window.addEventListener('pagehide', leavePage)
		window.addEventListener('pageshow', restorePage)
		return () => {
			window.clearTimeout(expiry)
			window.removeEventListener('pagehide', leavePage)
			window.removeEventListener('pageshow', restorePage)
			void lifecycle.dispose()
		}
	}, [kind, reference, html, baseDirectory, revision])

	useLayoutEffect(() => {
		if (!session) return
		const handshake = artifactHandshake()
		let loaded = false
		const timeout = window.setTimeout(() => {
			if (!loaded) setPhase('unavailable')
		}, 15_000)
		const receive = (event: MessageEvent) => {
			const message = artifactMessage(event, session, frame.current?.contentWindow ?? null)
			if (!message) return
			const action = handshake(message)
			if (action === 'authorize') {
				frame.current?.contentWindow?.postMessage(
					{
						channel: 'conductor-artifact-preview',
						version: 1,
						sessionId: session.id,
						type: 'authorize',
						grant: session.grant
					},
					session.origin
				)
				setPhase('authorizing')
			} else if (action === 'loaded') {
				loaded = true
				window.clearTimeout(timeout)
				setPhase('loaded')
			} else if (action === 'diagnostic' && message.type === 'diagnostic') {
				setDiagnostics(current => appendDiagnostic(current, { ...message, id: current.length }))
			}
		}
		window.addEventListener('message', receive)
		return () => {
			window.clearTimeout(timeout)
			window.removeEventListener('message', receive)
		}
	}, [session])

	return {
		frame,
		session,
		source,
		sourceError,
		error,
		phase,
		diagnostics,
		refresh: () => {
			setSession(null)
			setRevision(value => value + 1)
		}
	}
}
