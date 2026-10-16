import type { EffectCallback } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'

// Run the production effect without a browser DOM. React still owns the hook
// state; only effect scheduling and the external HTTP transport are controlled.
const effects = vi.hoisted(() => [] as EffectCallback[])
vi.mock('react', async importOriginal => ({
	...(await importOriginal<typeof import('react')>()),
	useEffect: (effect: EffectCallback) => effects.push(effect)
}))
vi.stubGlobal('location', { hash: '', pathname: '/', search: '' })
vi.stubGlobal('localStorage', { getItem: () => 'synthetic-hook-auth', setItem: () => {}, removeItem: () => {} })
const { useArtifactPreview } = await import('../../src/hooks/artifacts.ts')

const session = {
	id: 'owned-session',
	origin: 'http://localhost:41001',
	bootstrapUrl: 'http://localhost:41001/__conductor_preview/bootstrap?session=owned-session',
	entryPath: '/index.html',
	grant: 'synthetic-grant',
	expiresAt: '2026-10-16T12:30:00Z',
	path: null
}
let requests: string[]
let cleanup: ReturnType<EffectCallback>

function Probe() {
	useArtifactPreview({ kind: 'html', html: '<p>interactive state</p>' })
	return null
}

beforeEach(() => {
	vi.useFakeTimers({ toFake: ['Date', 'setTimeout', 'clearTimeout'] })
	vi.setSystemTime(new Date('2026-10-16T12:00:00Z'))
	vi.stubGlobal('location', { hash: '', pathname: '/', search: '' })
	vi.stubGlobal('localStorage', { getItem: () => 'synthetic-hook-auth', setItem: () => {}, removeItem: () => {} })
	effects.length = 0
	requests = []
	cleanup = undefined
	vi.stubGlobal(
		'window',
		Object.assign(new EventTarget(), {
			location: { origin: 'http://localhost:5173' },
			setTimeout,
			clearTimeout
		})
	)
	vi.stubGlobal(
		'fetch',
		vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
			const request = `${init?.method ?? 'GET'} ${input}`
			requests.push(request)
			if (request === 'POST /api/artifact-previews') return Response.json(session)
			if (request === 'GET /api/artifact-previews/owned-session/source') {
				return Response.json({ path: null, content: '<p>interactive state</p>' })
			}
			if (request === 'DELETE /api/artifact-previews/owned-session') return Response.json({ ok: true })
			throw new Error(`Unexpected request: ${request}`)
		})
	)
})

afterEach(async () => {
	if (typeof cleanup === 'function') cleanup()
	await Promise.resolve()
	vi.useRealTimers()
	vi.unstubAllGlobals()
})

it('leaves sliding idle expiry to the relay and releases the session only when its owner closes', async () => {
	renderToStaticMarkup(<Probe />)
	expect(effects).toHaveLength(1)
	cleanup = effects[0]()
	await vi.advanceTimersByTimeAsync(0)
	expect(requests).toEqual(['POST /api/artifact-previews', 'GET /api/artifact-previews/owned-session/source'])
	// The relay may renew its deadline on iframe resource requests, which the
	// parent cannot observe. Its initial expiresAt must not force a DELETE.
	await vi.advanceTimersByTimeAsync(1_800_001)
	expect(requests).toEqual(['POST /api/artifact-previews', 'GET /api/artifact-previews/owned-session/source'])
	if (typeof cleanup === 'function') cleanup()
	await Promise.resolve()
	expect(requests).toEqual([
		'POST /api/artifact-previews',
		'GET /api/artifact-previews/owned-session/source',
		'DELETE /api/artifact-previews/owned-session'
	])
})

it('still releases the owned session immediately when the page leaves', async () => {
	renderToStaticMarkup(<Probe />)
	cleanup = effects[0]()
	await vi.advanceTimersByTimeAsync(0)
	window.dispatchEvent(new Event('pagehide'))
	await Promise.resolve()
	expect(requests.at(-1)).toBe('DELETE /api/artifact-previews/owned-session')
})
