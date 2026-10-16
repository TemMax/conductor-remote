import { type ReactNode, useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { isHtmlArtifact } from '../../contract/shared.ts'
import { client } from '../../lib/api.ts'
import type { FilePreviewResponse } from '../../lib/types.ts'
import { PreviewTruncationNotice, SourceLines } from '../review/SourceLines.tsx'
import { ViewerHeader } from '../review/ViewerHeader.tsx'
import { Spinner } from '../ui.tsx'
import { ArtifactSheet } from './ArtifactViewer.tsx'

export function FilePreviewSheet({
	reference,
	onClose,
	renderMarkdown
}: {
	reference: string
	onClose: () => void
	renderMarkdown: (content: string) => ReactNode
}) {
	if (isHtmlArtifact(reference))
		return <ArtifactSheet key={reference} target={{ kind: 'file', reference }} onClose={onClose} />
	return <SourceFileSheet reference={reference} onClose={onClose} renderMarkdown={renderMarkdown} />
}

function SourceFileSheet({
	reference,
	onClose,
	renderMarkdown
}: {
	reference: string
	onClose: () => void
	renderMarkdown: (content: string) => ReactNode
}) {
	const [preview, setPreview] = useState<FilePreviewResponse | null>(null)
	const [error, setError] = useState<string | null>(null)
	const highlightedLine = useRef<HTMLDivElement>(null)

	useEffect(() => {
		let disposed = false
		setPreview(null)
		setError(null)
		void client.filePreview(reference).then(
			result => {
				if (!disposed) setPreview(result)
			},
			err => {
				if (!disposed) setError(err instanceof Error ? err.message : 'Could not read source file')
			}
		)
		return () => {
			disposed = true
		}
	}, [reference])

	useEffect(() => {
		if (!preview?.line) return
		highlightedLine.current?.scrollIntoView({ block: 'center' })
	}, [preview?.line])

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
				aria-label="Source file"
				className="fade-in pt-safe pb-safe fixed inset-0 z-[60] mx-auto flex flex-col bg-bg md:inset-6 md:rounded-3xl md:border md:border-border-soft"
			>
				<ViewerHeader
					title="Source"
					subtitle={preview?.path ?? reference}
					onClose={onClose}
					closeLabel="Close source preview"
				/>
				<div className="min-h-0 flex-1 overflow-auto">
					{!preview && !error ? <Spinner label="Reading source…" /> : null}
					{error ? <p className="mx-auto max-w-xs px-6 py-16 text-center text-sm text-muted">{error}</p> : null}
					{preview ? (
						isMarkdownFile(preview.path) ? (
							<MarkdownFile preview={preview} renderMarkdown={renderMarkdown} />
						) : (
							<SourceLines preview={preview} lineRef={highlightedLine} />
						)
					) : null}
				</div>
			</div>
		</>,
		document.body
	)
}

function isMarkdownFile(filePath: string): boolean {
	return filePath.toLowerCase().endsWith('.md')
}

function MarkdownFile({
	preview,
	renderMarkdown
}: {
	preview: FilePreviewResponse
	renderMarkdown: (content: string) => ReactNode
}) {
	return (
		<div className="p-4 text-sm leading-6 text-text">
			{renderMarkdown(preview.content)}
			<PreviewTruncationNotice preview={preview} />
		</div>
	)
}
