import { type ReactNode, useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { useArtifactPreview } from '../../hooks/artifacts.ts'
import { SourceLines } from '../review/SourceLines.tsx'
import { ViewerHeader } from '../review/ViewerHeader.tsx'
import { CopyButton, Spinner } from '../ui.tsx'
import type { ArtifactTarget } from './lifecycle.ts'
import { ARTIFACT_SANDBOX, MAX_DIAGNOSTICS } from './protocol.ts'

export function referenceLine(reference: string): number | null {
	const line = reference.match(/:([1-9]\d*)(?::\d+)?$/)?.[1]
	return line && Number.isSafeInteger(Number(line)) ? Number(line) : null
}

/** Shared full-height surface used by transcript sheets and Review All-files. */
export function ArtifactViewer({
	target,
	onClose,
	actions
}: {
	target: ArtifactTarget
	onClose: () => void
	actions?: ReactNode
}) {
	const preview = useArtifactPreview(target)
	const line = target.kind === 'file' ? referenceLine(target.reference) : null
	const [mode, setMode] = useState<'preview' | 'code'>(() => (line ? 'code' : 'preview'))
	const highlighted = useRef<HTMLDivElement>(null)
	useEffect(() => {
		if (mode === 'code' && preview.source && line) highlighted.current?.scrollIntoView({ block: 'center' })
	}, [mode, preview.source, line])
	const path = preview.source?.path ?? (target.kind === 'file' ? target.reference : 'Inline HTML')
	const buttonClass =
		'rounded-md px-2 py-1.5 text-xs text-muted hover:bg-surface-2 focus-visible:outline-2 focus-visible:outline-accent'
	return (
		<section className="flex min-h-0 flex-1 flex-col overflow-hidden bg-bg" aria-label="HTML artifact">
			<ViewerHeader title="HTML" subtitle={path} onClose={onClose} closeLabel="Close HTML preview" actions={actions} />
			<fieldset
				className="flex shrink-0 flex-wrap items-center gap-1 border-b border-border-soft px-3 py-1"
				aria-label="Artifact controls"
			>
				<button
					type="button"
					className={buttonClass}
					aria-pressed={mode === 'preview'}
					onClick={() => setMode('preview')}
				>
					Preview
				</button>
				<button type="button" className={buttonClass} aria-pressed={mode === 'code'} onClick={() => setMode('code')}>
					Code
				</button>
				<button type="button" className={buttonClass} onClick={preview.refresh}>
					Refresh
				</button>
				{preview.source ? <CopyButton text={preview.source.content} label="original HTML" size={14} /> : null}
			</fieldset>
			{preview.error ? (
				<div role="alert" className="shrink-0 px-4 py-3 text-sm text-del">
					{preview.error}{' '}
					<button type="button" className={buttonClass} onClick={preview.refresh}>
						Retry
					</button>
				</div>
			) : null}
			{mode === 'preview' && !preview.error && preview.phase !== 'loaded' ? (
				<div role="status" className="shrink-0 px-4 py-2 text-xs text-muted">
					{preview.phase === 'unavailable'
						? 'Preview diagnostics have not connected. The page may block instrumentation with its content security policy, or the preview could not load. Refresh to retry; Code shows the original source.'
						: preview.phase === 'creating'
							? 'Creating preview…'
							: 'Connecting preview…'}
				</div>
			) : null}
			{/* Keep the frame alive while reading Code, so toggling does not restart scripts. */}
			{preview.session && !preview.error ? (
				<iframe
					ref={preview.frame}
					key={preview.session.id}
					title="HTML artifact preview"
					src={preview.session.bootstrapUrl}
					sandbox={ARTIFACT_SANDBOX}
					referrerPolicy="no-referrer"
					className={mode === 'preview' ? 'min-h-0 w-full flex-1 border-0 bg-white' : 'hidden'}
				/>
			) : null}
			{mode === 'code' ? (
				<div className="min-h-0 flex-1 overflow-auto overscroll-contain">
					{preview.sourceError ? (
						<p role="alert" className="px-4 py-3 text-sm text-del">
							{preview.sourceError}{' '}
							<button type="button" onClick={preview.refresh}>
								Retry
							</button>
						</p>
					) : null}
					{!preview.source && !preview.sourceError && !preview.error ? (
						<Spinner label="Reading original source…" />
					) : null}
					{preview.source ? (
						<SourceLines
							lineRef={highlighted}
							preview={{
								path: preview.source.path ?? 'inline.html',
								content: preview.source.content,
								line,
								lineStart: 1,
								lineEnd: preview.source.content.split('\n').length,
								totalLines: preview.source.content.split('\n').length,
								truncated: false
							}}
						/>
					) : null}
				</div>
			) : null}
			<details
				className="max-h-[30dvh] shrink-0 overflow-auto border-t border-border-soft text-xs text-muted"
				open={preview.diagnostics.length > 0}
			>
				<summary className="cursor-pointer px-4 py-2">
					Diagnostics ({preview.diagnostics.length}
					{preview.diagnostics.length === MAX_DIAGNOSTICS ? '+' : ''})
				</summary>
				{!preview.diagnostics.length ? (
					<p className="px-4 pb-3">
						{preview.phase === 'loaded' ? 'No errors reported.' : 'Diagnostics are not connected yet.'}
					</p>
				) : null}
				<ul className="space-y-2 px-4 pb-3">
					{preview.diagnostics.map(diagnostic => (
						<li key={diagnostic.id} className="whitespace-pre-wrap break-words">
							<strong>{diagnostic.kind}</strong>: {diagnostic.message}
							{diagnostic.source ? (
								<span className="block font-mono">
									{diagnostic.source}
									{diagnostic.line !== undefined ? `:${diagnostic.line}` : ''}
									{diagnostic.column !== undefined ? `:${diagnostic.column}` : ''}
								</span>
							) : null}
						</li>
					))}
				</ul>
			</details>
		</section>
	)
}

export function ArtifactSheet({ target, onClose }: { target: ArtifactTarget; onClose: () => void }) {
	useEffect(() => {
		const closeOnEscape = (event: KeyboardEvent) => {
			if (event.key === 'Escape') onClose()
		}
		window.addEventListener('keydown', closeOnEscape)
		return () => window.removeEventListener('keydown', closeOnEscape)
	}, [onClose])
	return createPortal(
		<>
			<div className="fixed inset-0 z-[60] bg-black/60" onClick={onClose} aria-hidden />
			<div
				role="dialog"
				aria-modal="true"
				aria-label="HTML preview"
				className="fade-in pt-safe pb-safe fixed inset-0 z-[60] mx-auto flex flex-col bg-bg md:inset-6 md:rounded-3xl md:border md:border-border-soft"
			>
				<ArtifactViewer target={target} onClose={onClose} />
			</div>
		</>,
		document.body
	)
}
