import type { ArtifactPreviewSession, ArtifactPreviewSource, CreateArtifactPreviewRequest } from '../../lib/types.ts'
import { isolatedSession } from './protocol.ts'

export type ArtifactTarget =
	| { kind: 'file'; reference: string }
	| { kind: 'html'; html: string; baseDirectory?: string | null }

export interface ArtifactApi {
	createArtifactPreview: (request: CreateArtifactPreviewRequest) => Promise<ArtifactPreviewSession>
	artifactPreviewSource: (id: string) => Promise<ArtifactPreviewSource>
	closeArtifactPreview: (id: string) => Promise<unknown>
}

/** One effect owns one session, including sessions whose creation resolves after cleanup. */
export function startArtifact(
	api: ArtifactApi,
	target: ArtifactTarget,
	parentOrigin: string,
	onSession: (session: ArtifactPreviewSession) => void,
	onSource: (source: ArtifactPreviewSource) => void,
	onError: (message: string) => void,
	onSourceError: (message: string) => void
) {
	let disposed = false
	let owned: ArtifactPreviewSession | null = null
	let closing: Promise<void> | null = null
	const close = (session: ArtifactPreviewSession) => {
		closing ??= api.closeArtifactPreview(session.id).then(
			() => {},
			() => {}
		)
		return closing
	}
	const settled = (async () => {
		try {
			const session = await api.createArtifactPreview({ ...target, parentOrigin })
			owned = session
			if (disposed) {
				await close(session)
				return
			}
			if (!isolatedSession(session, parentOrigin)) {
				await close(session)
				if (!disposed) onError('The preview server returned an invalid isolated origin. Retry the preview.')
				return
			}
			onSession(session)
			try {
				const source = await api.artifactPreviewSource(session.id)
				if (!disposed) onSource(source)
			} catch (error) {
				if (!disposed) onSourceError(error instanceof Error ? error.message : 'Could not read original source')
			}
		} catch (error) {
			if (!disposed) onError(error instanceof Error ? error.message : 'Could not create preview')
		}
	})()
	return {
		settled,
		dispose: () => {
			disposed = true
			return owned ? close(owned) : settled.then(() => {})
		}
	}
}
