import { renderToStaticMarkup } from 'react-dom/server'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { useArtifactPreview } from '../../src/hooks/artifacts.ts'

let state: ReturnType<typeof useArtifactPreview>
vi.mock('../../src/hooks/artifacts.ts', () => ({ useArtifactPreview: () => state }))
vi.stubGlobal('location', { hash: '', pathname: '/', search: '' })
vi.stubGlobal('localStorage', { getItem: () => null, setItem: () => {}, removeItem: () => {} })
vi.stubGlobal('history', { replaceState: () => {} })
const { ArtifactViewer } = await import('../../src/components/artifacts/ArtifactViewer.tsx')
const content = `<!DOCTYPE html>\n${'<!-- original line -->\n'.repeat(600)}<button onclick="run()">final marker</button>\n`
const render = (reference = '/work/page.htm:602:4') =>
	renderToStaticMarkup(<ArtifactViewer target={{ kind: 'file', reference }} onClose={() => {}} />)

beforeEach(() => {
	state = {
		frame: { current: null },
		session: {
			id: 'one',
			origin: 'http://localhost:31001',
			bootstrapUrl: 'http://localhost:31001/__conductor_preview/bootstrap?sessionId=one',
			entryPath: '/page.htm',
			grant: 'synthetic-grant',
			expiresAt: '2099-01-01T00:00:00Z',
			path: '/work/page.htm'
		},
		source: { path: '/work/page.htm', content },
		sourceError: null,
		error: null,
		phase: 'loaded',
		diagnostics: [],
		refresh: vi.fn()
	}
})

describe('shared artifact surface (static rendering)', () => {
	it('shows original full source beyond line 500 and selects the explicit line', () => {
		const markup = render()
		expect(markup).toContain('aria-pressed="true">Code</button>')
		expect(markup).toContain('final marker')
		expect(markup).toContain('>602</span>')
		expect(markup).toContain('rounded bg-accent-soft text-text')
		expect(markup).not.toContain('Showing lines')
		expect(markup).not.toContain('<button onclick="run()">')
		expect(markup).toContain('aria-label="Copy original HTML"')
	})

	it('uses the isolated bootstrap URL and minimal sandbox, never a grant in iframe markup', () => {
		const markup = render('/work/page.htm')
		expect(markup).toContain('aria-pressed="true">Preview</button>')
		expect(markup).toContain('src="http://localhost:31001/__conductor_preview/bootstrap?sessionId=one"')
		expect(markup).toContain('sandbox="allow-scripts allow-same-origin"')
		expect(markup).toContain('referrerPolicy="no-referrer"')
		expect(markup).not.toContain('synthetic-grant')
		expect(markup).not.toContain('allow-top-navigation')
	})

	it('displays diagnostics as escaped text with source locations', () => {
		state.diagnostics = [
			{ id: 0, kind: 'javascript', message: '<img src=x onerror=run()>', source: '/app.js', line: 12, column: 3 }
		]
		const markup = render('/work/page.htm')
		expect(markup).toContain('Diagnostics (1)')
		expect(markup).toContain('&lt;img src=x onerror=run()&gt;')
		expect(markup).toContain('/app.js:12:3')
		expect(markup).not.toContain('<img')
		expect(markup).toMatch(/<details[^>]+open=""/)
	})

	it('shows errors and Retry without hiding original source', () => {
		state.error = 'Preview unavailable'
		const markup = render()
		expect(markup).toContain('role="alert"')
		expect(markup).toContain('Preview unavailable')
		expect(markup).toContain('>Retry</button>')
		expect(markup).toContain('final marker')
		expect(markup).not.toContain('<iframe')
	})

	it('states instrumentation uncertainty honestly instead of claiming a successful load', () => {
		state.phase = 'unavailable'
		const markup = render('/work/page.htm')
		expect(markup).toContain('Preview diagnostics have not connected.')
		expect(markup).toContain('content security policy')
		expect(markup).not.toContain('No errors reported.')
	})
})
