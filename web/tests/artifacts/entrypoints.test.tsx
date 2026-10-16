import { readFileSync } from 'node:fs'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import type { ReactNode } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it, vi } from 'vitest'
import { isHtmlArtifact, isPreviewableSource } from '../../src/contract/shared.ts'
import {
	buildResolver,
	MentionResolverProvider,
	resolveAttachmentReference,
	resolveHtmlReference
} from '../../src/lib/fileMentions.ts'
import type { Workspace } from '../../src/lib/types.ts'

vi.mock('../../src/lib/api.ts', () => ({
	client: {
		createArtifactPreview: vi.fn(),
		artifactPreviewSource: vi.fn(),
		closeArtifactPreview: vi.fn(),
		filePreview: vi.fn()
	}
}))
vi.mock('react-dom', () => ({ createPortal: (children: ReactNode) => children }))
vi.stubGlobal('document', { body: {} })
vi.stubGlobal('location', { hash: '', pathname: '/', search: '' })
vi.stubGlobal('localStorage', { getItem: () => null, setItem: () => {}, removeItem: () => {} })
vi.stubGlobal('history', { replaceState: () => {} })
const { client } = await import('../../src/lib/api.ts')
const { ArtifactViewer } = await import('../../src/components/artifacts/ArtifactViewer.tsx')
const { FilePreviewSheet } = await import('../../src/components/artifacts/FilePreviewSheet.tsx')
const { Markdown } = await import('../../src/components/transcript/Markdown.tsx')
const { DiffFileViewer } = await import('../../src/components/review/DiffView.tsx')
const code = (path: string) => readFileSync(new URL(`../../src/${path}`, import.meta.url), 'utf8')
const worktree = '/workspace'
const renderChat = (text: string) =>
	renderToStaticMarkup(
		<MentionResolverProvider value={{ resolveMention: buildResolver(worktree, ['pages/page.htm']), worktree }}>
			<Markdown>{text}</Markdown>
		</MentionResolverProvider>
	)

describe('production artifact entrypoints (static rendering, no browser execution)', () => {
	it('only classifies HTML as executable, preserving source eligibility for other formats', () => {
		for (const reference of ['/workspace/page.html', '/workspace/page.HTM:601:2'])
			expect(isHtmlArtifact(reference)).toBe(true)
		for (const reference of ['page.svg', 'page.xml', 'page.md', 'page.ts', 'page.html.js', 'page.html?x=1'])
			expect(isHtmlArtifact(reference)).toBe(false)
		expect(isPreviewableSource('page.htm')).toBe(true)
		expect(isPreviewableSource('page.svg')).toBe(true)
	})

	it('resolves explicit ignored HTML destinations without broadening prose mentions', () => {
		expect(resolveHtmlReference('./.context/artifacts/my%20page.htm:601:2', worktree)).toBe(
			'/workspace/.context/artifacts/my page.htm:601:2'
		)
		for (const reference of [
			'../page.html',
			'%2e%2e/page.html',
			'.context/../page.html',
			'//host/page.html',
			'https://host/page.html',
			'file:///page.html',
			'page.html?query',
			'a%00.html',
			'a%5cpage.html',
			'%zz.html'
		]) {
			expect(resolveHtmlReference(reference, worktree)).toBeNull()
		}
		expect(resolveHtmlReference('page.html', null)).toBeNull()
		expect(buildResolver(worktree, [])('.context/artifacts/page.html')).toBeNull()
		expect(buildResolver(worktree, ['a/page.html', 'b/page.html'])('page.html')).toBeNull()
		expect(buildResolver(worktree, ['a/page.html'])('page.html:20')).toBe('/workspace/a/page.html:20')
	})

	it('renders real Markdown links, mentions and attachments as local viewer controls', () => {
		expect(renderChat('[artifact](.context/artifacts/page.html)')).toContain(
			'title="Open /workspace/.context/artifacts/page.html"'
		)
		expect(renderChat('Open `pages/page.htm:601`')).toContain('title="Open /workspace/pages/page.htm:601"')
		const attachment = '.context/attachments/abc123/page.html'
		expect(resolveAttachmentReference(attachment, worktree)).toEqual({
			reference: `/workspace/${attachment}`,
			kind: 'source'
		})
		expect(renderChat(`@⟦page.html⟧(${encodeURIComponent(attachment)})`)).toContain(
			`title="Open /workspace/${attachment}"`
		)
		expect(renderChat('Use `Array.map` and `missing.html`')).not.toContain('title="Open')
	})

	it('adds a lazy Preview button only for raw HTML fence classes and keeps Copy', () => {
		for (const language of ['html', 'htm']) {
			const markup = renderChat(`\`\`\`${language}\n<!DOCTYPE html>\n<button>run</button>\n\`\`\``)
			expect(markup).toContain('aria-label="Preview HTML"')
			expect(markup).toContain('aria-label="Copy code"')
			expect(markup).not.toContain('<iframe')
			expect(markup).not.toContain('<button>run</button>')
		}
		for (const language of ['', 'xml', 'svg', 'js', 'htmlish']) {
			expect(renderChat(`\`\`\`${language}\n<p>code</p>\n\`\`\``)).not.toContain('aria-label="Preview HTML"')
		}
		expect(client.createArtifactPreview).not.toHaveBeenCalled()
	})

	it('routes HTML source sheets into the shared viewer and opens line references in Code', () => {
		for (const reference of ['/workspace/page.html', '/workspace/page.htm:601:2']) {
			const markup = renderToStaticMarkup(
				<FilePreviewSheet reference={reference} onClose={() => {}} renderMarkdown={text => text} />
			)
			expect(markup).toContain('aria-label="HTML artifact"')
			expect(markup).toContain('>Refresh</button>')
			expect(markup).toContain(`aria-pressed="${reference.endsWith(':2') ? 'true' : 'false'}">Code</button>`)
		}
		const plain = renderToStaticMarkup(
			<FilePreviewSheet reference="/workspace/file.ts" onClose={() => {}} renderMarkdown={text => text} />
		)
		expect(plain).toContain('aria-label="Source file"')
		expect(plain).not.toContain('aria-label="HTML artifact"')
	})

	it('keeps Review navigation with the shared HTML surface and leaves Changes as a diff', () => {
		const review = {
			workspace: { id: 'work', worktree } as Workspace,
			query: { data: undefined, isLoading: false, isError: false, error: null },
			filesQuery: {
				data: { files: ['page.htm', 'other.ts'], truncated: false },
				isLoading: false,
				isError: false,
				error: null
			},
			fileQuery: { data: undefined, isLoading: false, isError: false, error: null }
		}
		const render = (scope: 'all' | 'changed') =>
			renderToStaticMarkup(
				<QueryClientProvider client={new QueryClient()}>
					<DiffFileViewer
						review={review}
						filePath="page.htm"
						scope={scope}
						showFolders
						onSelectFile={() => {}}
						onShowFiles={() => {}}
						onClose={() => {}}
					/>
				</QueryClientProvider>
			)
		const all = render('all')
		expect(all).toContain('aria-label="HTML artifact"')
		for (const label of ['Previous file', 'Next file', 'Show all files', 'Close HTML preview'])
			expect(all).toContain(`aria-label="${label}"`)
		expect(render('changed')).toContain('aria-label="Changes in page.htm"')
		expect(render('changed')).not.toContain('aria-label="HTML artifact"')
	})

	it('pins lazy production wiring, exact inline source/base, isolation and cleanup call sites', () => {
		const markdown = code('components/transcript/Markdown.tsx')
		expect(markdown).toMatch(/previewing \?\s*\(\s*<FilePreviewSheet/)
		expect(markdown).toContain('renderMarkdown={renderMarkdownFile}')
		expect(markdown).toContain("target={{ kind: 'html', html: source, baseDirectory }}")
		expect(markdown).toContain('useArtifactBaseDirectory()')
		expect(markdown).toContain('const source = node ? fenceSource(node)')
		const tools = code('components/transcript/ToolEntry.tsx')
		expect(tools).toContain("import { ChatLink, sourceReference } from './Markdown.tsx'")
		expect(tools).toContain('<ChatLink')
		const source = code('components/artifacts/FilePreviewSheet.tsx')
		expect(source).toContain('if (isHtmlArtifact(reference))')
		expect(source).toContain('renderMarkdown(preview.content)')
		for (const file of ['ArtifactViewer.tsx', 'FilePreviewSheet.tsx']) {
			expect(code(`components/artifacts/${file}`)).not.toMatch(/from .*Markdown/)
		}
		const viewer = code('components/artifacts/ArtifactViewer.tsx')
		expect(viewer).toContain('sandbox={ARTIFACT_SANDBOX}')
		expect(viewer).toContain('src={preview.session.bootstrapUrl}')
		expect(viewer).not.toMatch(/srcDoc|dangerouslySetInnerHTML|getToken|localStorage/)
		const hook = code('hooks/artifacts.ts')
		expect(hook).toContain('artifactMessage(event, session, frame.current?.contentWindow ?? null)')
		expect(hook).toMatch(/},\s*session\.origin\s*\)/)
		expect(hook).toContain('window.removeEventListener')
		expect(hook).toContain("window.addEventListener('pagehide', leavePage)")
		expect(hook).toContain("window.removeEventListener('pagehide', leavePage)")
		expect(hook).toContain('if (event.persisted) setRevision')
		expect(hook).toContain('lifecycle.dispose()')
		expect(hook).not.toMatch(/setInterval|refetchInterval|postMessage\([^;]*['"]\*['"]/)
		const diff = code('components/review/DiffView.tsx')
		expect(diff).toContain("useFilePreview(sourceReference, scope === 'all' && !htmlArtifact)")
		expect(diff).toContain('key={sourceReference}')
	})

	it('keeps the initial viewer full-height and the frame lazy', () => {
		const markup = renderToStaticMarkup(
			<ArtifactViewer
				target={{ kind: 'html', html: '<p>unexecuted</p>', baseDirectory: worktree }}
				onClose={() => {}}
			/>
		)
		expect(markup).toContain('min-h-0 flex-1 flex-col')
		expect(markup).toContain('Creating preview…')
		expect(markup).toContain('Diagnostics (0)')
		expect(markup).not.toContain('<iframe')
	})
})
