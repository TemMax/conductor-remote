import { readFileSync } from 'node:fs'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import type { ReactNode } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ArtifactTarget } from '../../src/components/artifacts/lifecycle.ts'
import { startArtifact } from '../../src/components/artifacts/lifecycle.ts'
import { buildResolver, MentionResolverProvider } from '../../src/lib/fileMentions.ts'
import type { Workspace } from '../../src/lib/types.ts'

// Preopen the production controls during server rendering, then observe the real viewer
// hook's input. This exercises conditional branches without claiming clicks, effects,
// iframe execution or a browser pass. The client transport is tested separately below.
const harness = vi.hoisted(() => ({ open: false, targets: [] as ArtifactTarget[] }))
vi.mock('react', async importOriginal => {
	const react = await importOriginal<typeof import('react')>()
	return {
		...react,
		useState: (initial: unknown) => react.useState(harness.open && initial === false ? true : initial)
	}
})
vi.mock('react-dom', () => ({ createPortal: (children: ReactNode) => children }))
vi.mock('../../src/hooks/artifacts.ts', async importOriginal => {
	const hooks = await importOriginal<typeof import('../../src/hooks/artifacts.ts')>()
	return {
		...hooks,
		useArtifactPreview: (target: ArtifactTarget) => {
			harness.targets.push(target)
			return hooks.useArtifactPreview(target)
		}
	}
})
vi.stubGlobal('document', { body: {} })
vi.stubGlobal('location', { hash: '', pathname: '/', search: '' })
vi.stubGlobal('localStorage', { getItem: () => 'synthetic-acceptance-auth', setItem: () => {}, removeItem: () => {} })
vi.stubGlobal('history', { replaceState: () => {} })
const { Markdown } = await import('../../src/components/transcript/Markdown.tsx')
const { DiffFileViewer } = await import('../../src/components/review/DiffView.tsx')
const { client } = await import('../../src/lib/api.ts')
const worktree = '/workspace'
const parentOrigin = 'http://localhost:5173'
const original = `<!DOCTYPE html>\r\n${'<!-- original -->\n'.repeat(700)}<button>final marker</button>\n`
const read = (path: string) => readFileSync(new URL(`../../src/${path}`, import.meta.url), 'utf8')
const chat = (text: string) => (
	<MentionResolverProvider
		value={{ resolveMention: buildResolver(worktree, ['page.htm', 'notes.md', 'source.ts']), worktree }}
	>
		<Markdown>{text}</Markdown>
	</MentionResolverProvider>
)
const review = (filePath: string, scope: 'all' | 'changed') => (
	<QueryClientProvider client={new QueryClient()}>
		<DiffFileViewer
			review={{
				workspace: { id: 'fixture', worktree } as Workspace,
				query: { data: undefined, isLoading: false, isError: false, error: null },
				filesQuery: {
					data: { files: ['page.htm', 'source.ts'], truncated: false },
					isLoading: false,
					isError: false,
					error: null
				},
				fileQuery: { data: undefined, isLoading: false, isError: false, error: null }
			}}
			filePath={filePath}
			scope={scope}
			showFolders
			onSelectFile={() => {}}
			onShowFiles={() => {}}
			onClose={() => {}}
		/>
	</QueryClientProvider>
)

beforeEach(() => {
	harness.open = true
	harness.targets = []
	vi.stubGlobal('fetch', vi.fn())
})

describe('production branches → shared viewer → real client (no browser execution)', () => {
	it.each([
		[
			'ChatLink',
			() => chat('[open](.context/page.html)'),
			{ kind: 'file', reference: '/workspace/.context/page.html' }
		],
		['FileMention', () => chat('Open `page.htm:650:2`'), { kind: 'file', reference: '/workspace/page.htm:650:2' }],
		[
			'HTML fence',
			() => chat(`\`\`\`html\n${original}\`\`\``),
			// The fence delimiter's final newline is excluded; all document bytes survive.
			{ kind: 'html', html: original.replace(/\n$/, ''), baseDirectory: worktree }
		],
		['Review All-files', () => review('page.htm', 'all'), { kind: 'file', reference: '/workspace/page.htm' }]
	] as const)('%s supplies its exact target to the shared hook and authenticated client', async (_, entry, target) => {
		const markup = renderToStaticMarkup(entry())
		expect(markup).toContain('aria-label="HTML artifact"')
		expect(markup).toContain('>Refresh</button>')
		expect(markup).toContain(
			`aria-pressed="${target.kind === 'file' && target.reference.includes(':650')}">Code</button>`
		)
		expect(harness.targets).toEqual([target])
		expect(fetch).not.toHaveBeenCalled() // Server rendering is lazy.

		const session = {
			id: 'acceptance-session',
			origin: 'http://localhost:31001',
			bootstrapUrl: 'http://localhost:31001/__conductor_preview/bootstrap?session=acceptance-session',
			entryPath: '/index.htm',
			grant: 'synthetic-acceptance-grant',
			expiresAt: '2099-01-01T00:00:00Z',
			path: target.kind === 'file' ? target.reference : null
		}
		const source = { path: session.path, content: original }
		const transport = vi.mocked(fetch).mockImplementation(async (input, init) => {
			const path = String(input)
			expect(new Headers(init?.headers).get('authorization')).toBe('Bearer synthetic-acceptance-auth')
			if (init?.method === 'POST') {
				expect(path).toBe('/api/artifact-previews')
				expect(JSON.parse(String(init.body))).toEqual({ ...target, parentOrigin })
				return Response.json(session)
			}
			if (init?.method === 'DELETE') {
				expect(path).toBe('/api/artifact-previews/acceptance-session')
				return Response.json({ ok: true })
			}
			expect(path).toBe('/api/artifact-previews/acceptance-session/source')
			return Response.json(source)
		})
		const onSession = vi.fn()
		const onSource = vi.fn()
		const onError = vi.fn()
		const onSourceError = vi.fn()
		const lifecycle = startArtifact(
			client,
			harness.targets[0],
			parentOrigin,
			onSession,
			onSource,
			onError,
			onSourceError
		)
		await lifecycle.settled
		expect(onSession).toHaveBeenCalledExactlyOnceWith(session)
		expect(onSource).toHaveBeenCalledExactlyOnceWith(source)
		expect(onError).not.toHaveBeenCalled()
		expect(onSourceError).not.toHaveBeenCalled()
		await lifecycle.dispose()
		expect(transport.mock.calls.map(([path, init]) => [path, init?.method ?? 'GET'])).toEqual([
			['/api/artifact-previews', 'POST'],
			['/api/artifact-previews/acceptance-session/source', 'GET'],
			['/api/artifact-previews/acceptance-session', 'DELETE']
		])
	})

	it('retains source, Markdown, raster and Changes branches without creating artifact sessions', () => {
		for (const text of ['[source](source.ts)', 'Read `notes.md`']) {
			const markup = renderToStaticMarkup(chat(text))
			expect(markup).toContain('aria-label="Source file"')
			expect(markup).not.toContain('aria-label="HTML artifact"')
		}
		const image = renderToStaticMarkup(chat('[image](.context/image.png)'))
		expect(image).toContain('aria-label="Image preview"')
		expect(image).not.toContain('aria-label="HTML artifact"')
		expect(renderToStaticMarkup(review('source.ts', 'all'))).toContain('aria-label="Source of source.ts"')
		expect(renderToStaticMarkup(review('page.htm', 'changed'))).toContain('aria-label="Changes in page.htm"')
		// Non-HTML fences have no Preview control to open.
		harness.open = false
		for (const language of ['xml', 'svg', 'js']) {
			expect(renderToStaticMarkup(chat(`\`\`\`${language}\n<p>code</p>\n\`\`\``))).not.toContain('Preview HTML')
		}
		expect(harness.targets).toEqual([])
		expect(fetch).not.toHaveBeenCalled()
		// Pin the existing rendered Markdown and raster clients, not merely an empty loading sheet.
		expect(read('components/artifacts/FilePreviewSheet.tsx')).toContain('renderMarkdown(preview.content)')
		expect(read('hooks/images.ts')).toContain('client.localImage(source)')
	})

	it('connects the viewer hook to the tested lifecycle and preserves transcript memoization', () => {
		const hook = read('hooks/artifacts.ts')
		expect(hook).toMatch(/const lifecycle = startArtifact\(\s*client,/)
		expect(hook).toContain('void lifecycle.dispose()')
		expect(hook).toContain('window.location.origin')
		expect(read('components/artifacts/ArtifactViewer.tsx')).toContain('useArtifactPreview(target)')
		expect(read('components/transcript/Markdown.tsx')).toContain('export const Markdown = memo(')
	})
})
