import { readFileSync } from 'node:fs'
import { afterEach, describe, expect, test, vi } from 'vitest'
import { isRoute, routeParam, routes } from '../../src/contract/routes.ts'
import { client } from '../../src/lib/api.ts'
import type {
	ArtifactPreviewMessage,
	ArtifactPreviewSession,
	ArtifactPreviewSource,
	CloseArtifactPreviewResult,
	CreateArtifactPreviewRequest,
	FilePreviewResponse
} from '../../src/lib/types.ts'

vi.mock('../../src/lib/auth-token.ts', () => ({ getToken: () => 'synthetic-auth' }))
afterEach(() => vi.unstubAllGlobals())

const rust = readFileSync(new URL('../../../crates/relay/tests/artifact_contracts.rs', import.meta.url), 'utf8')
function fixture(name: string): unknown {
	const literal = rust.match(new RegExp(`const ${name}: &str = r#"([\\s\\S]*?)"#;`))?.[1]
	if (!literal) throw new Error(`missing shared Rust fixture ${name}`)
	return JSON.parse(literal)
}

const requests: CreateArtifactPreviewRequest[] = [
	{ kind: 'file', reference: '/workspace/artifacts/page.htm:601:3', parentOrigin: 'http://localhost:8790' },
	{ kind: 'html', html: '<!DOCTYPE html>\n<p>inline</p>', parentOrigin: 'http://localhost:5173' },
	{
		kind: 'html',
		html: '<p>with dependencies</p>',
		baseDirectory: '/workspace/artifacts',
		parentOrigin: 'https://relay.example.test'
	}
]
const sessions: ArtifactPreviewSession[] = [
	{
		id: 'preview-one',
		origin: 'http://localhost:31001',
		bootstrapUrl: 'http://localhost:31001/__conductor_preview/bootstrap',
		entryPath: '/page.htm',
		grant: 'synthetic-grant',
		expiresAt: '2026-10-16T12:30:00Z',
		path: '/workspace/artifacts/page.htm'
	},
	{
		id: 'preview-two',
		origin: 'https://relay.example.test:31002',
		bootstrapUrl: 'https://relay.example.test:31002/__conductor_preview/bootstrap',
		entryPath: '/index.html',
		grant: 'synthetic-grant-two',
		expiresAt: '2026-10-16T12:30:00Z',
		path: null
	}
]
const sources: ArtifactPreviewSource[] = [
	{ path: '/workspace/artifacts/page.htm', content: '<!DOCTYPE html>\n<html><body>original</body></html>' },
	{ path: null, content: '<p>inline</p>' }
]
const closed: CloseArtifactPreviewResult = { ok: true }
const envelope = { channel: 'conductor-artifact-preview', version: 1, sessionId: 'preview-one' } as const
const messages: ArtifactPreviewMessage[] = [
	{ ...envelope, type: 'ready' },
	{ ...envelope, type: 'authorize', grant: 'synthetic-grant' },
	{ ...envelope, type: 'loaded' },
	{
		...envelope,
		type: 'diagnostic',
		kind: 'javascript',
		message: 'synthetic failure',
		source: '/app.js',
		line: 12,
		column: 3
	},
	{ ...envelope, type: 'diagnostic', kind: 'promise', message: 'synthetic rejection' },
	{ ...envelope, type: 'diagnostic', kind: 'resource', message: 'missing image', source: '/missing.png' },
	{ ...envelope, type: 'diagnostic', kind: 'http', message: '404', source: '/missing.css' }
]

describe('artifact preview contract', () => {
	test('TypeScript DTOs and iframe messages match Rust serde fixtures', () => {
		for (const [name, value] of [
			['REQUESTS_JSON', requests],
			['SESSIONS_JSON', sessions],
			['SOURCES_JSON', sources],
			['CLOSE_JSON', closed],
			['MESSAGES_JSON', messages]
		] as const) {
			expect(fixture(name)).toStrictEqual(value)
			expect(JSON.parse(JSON.stringify(value))).toStrictEqual(value)
		}
	})

	test('pins exact routes and preserves encoded IDs and method identity', () => {
		expect(routes.createArtifactPreview.method).toBe('POST')
		expect(routes.createArtifactPreview.path()).toBe('/api/artifact-previews')
		expect(isRoute(routes.createArtifactPreview, 'POST', '/api/artifact-previews')).toBe(true)
		expect(isRoute(routes.createArtifactPreview, 'GET', '/api/artifact-previews')).toBe(false)
		for (const id of ['preview-one', 'a/b ?#', 'Ünicode']) {
			const encoded = encodeURIComponent(id)
			expect(routes.artifactPreviewSource.path(id)).toBe(`/api/artifact-previews/${encoded}/source`)
			expect(routes.closeArtifactPreview.path(id)).toBe(`/api/artifact-previews/${encoded}`)
			expect(routeParam(routes.artifactPreviewSource, 'GET', routes.artifactPreviewSource.path(id))).toBe(id)
			expect(routeParam(routes.closeArtifactPreview, 'DELETE', routes.closeArtifactPreview.path(id))).toBe(id)
		}
		expect(routeParam(routes.artifactPreviewSource, 'GET', '/api/artifact-previews/x')).toBeNull()
		expect(routeParam(routes.closeArtifactPreview, 'GET', '/api/artifact-previews/x')).toBeNull()
		expect(routeParam(routes.closeArtifactPreview, 'DELETE', '/api/artifact-previews/x/source')).toBeNull()
	})

	test('client uses the authenticated control routes with the exact DTOs', async () => {
		const fetch = vi.fn<typeof globalThis.fetch>()
		vi.stubGlobal('fetch', fetch)
		for (const request of [
			...requests,
			{ kind: 'html', html: '<p>null base</p>', baseDirectory: null, parentOrigin: 'http://localhost:8790' }
		] satisfies CreateArtifactPreviewRequest[]) {
			fetch.mockResolvedValueOnce(Response.json(sessions[0]))
			expect(await client.createArtifactPreview(request)).toStrictEqual(sessions[0])
			expect(fetch).toHaveBeenLastCalledWith(
				'/api/artifact-previews',
				expect.objectContaining({
					method: 'POST',
					body: JSON.stringify(request),
					headers: expect.objectContaining({ authorization: 'Bearer synthetic-auth' })
				})
			)
		}
		const full: ArtifactPreviewSource = {
			path: null,
			content: `<!DOCTYPE html>\r\n${'line\n'.repeat(601)}<p>last marker</p>`
		}
		fetch.mockResolvedValueOnce(Response.json(full))
		expect(await client.artifactPreviewSource('a/b')).toStrictEqual(full)
		expect(fetch).toHaveBeenLastCalledWith(
			'/api/artifact-previews/a%2Fb/source',
			expect.objectContaining({
				headers: expect.objectContaining({ authorization: 'Bearer synthetic-auth' })
			})
		)
		fetch.mockResolvedValueOnce(Response.json(closed))
		expect(await client.closeArtifactPreview('a/b')).toStrictEqual(closed)
		expect(fetch).toHaveBeenLastCalledWith(
			'/api/artifact-previews/a%2Fb',
			expect.objectContaining({ method: 'DELETE' })
		)
	})

	test('keeps the bounded file-preview API and wire shape', async () => {
		const preview: FilePreviewResponse = {
			path: '/workspace/page.htm',
			line: 601,
			lineStart: 501,
			lineEnd: 602,
			totalLines: 602,
			content: 'excerpt',
			truncated: true
		}
		const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValue(Response.json(preview))
		vi.stubGlobal('fetch', fetch)
		expect(await client.filePreview('/workspace/page.htm:601')).toStrictEqual(preview)
		expect(fetch).toHaveBeenCalledWith('/api/files/%2Fworkspace%2Fpage.htm%3A601', expect.anything())
	})
})
