import { describe, expect, it, vi } from 'vitest'
import { type ArtifactApi, type ArtifactTarget, startArtifact } from '../../src/components/artifacts/lifecycle.ts'
import type { ArtifactPreviewSession, ArtifactPreviewSource } from '../../src/lib/types.ts'

function deferred<T>() {
	let resolve!: (value: T) => void
	let reject!: (reason: Error) => void
	const promise = new Promise<T>((yes, no) => {
		resolve = yes
		reject = no
	})
	return { promise, resolve, reject }
}
const session: ArtifactPreviewSession = {
	id: 'one',
	origin: 'http://localhost:31001',
	bootstrapUrl: 'http://localhost:31001/__conductor_preview/bootstrap',
	entryPath: '/page.htm',
	grant: 'synthetic',
	expiresAt: '2099-01-01T00:00:00Z',
	path: '/work/page.htm'
}
const source = {
	path: session.path,
	content: `<!DOCTYPE html>\r\n${'line\n'.repeat(601)}<button>final marker</button>\n`
}
const target: ArtifactTarget = { kind: 'file', reference: '/work/page.htm:601:2' }
const parentOrigin = 'http://localhost:5173'
function setup() {
	const create = deferred<ArtifactPreviewSession>()
	const read = deferred<ArtifactPreviewSource>()
	const api = {
		createArtifactPreview: vi.fn<ArtifactApi['createArtifactPreview']>(() => create.promise),
		artifactPreviewSource: vi.fn<ArtifactApi['artifactPreviewSource']>(() => read.promise),
		closeArtifactPreview: vi.fn<ArtifactApi['closeArtifactPreview']>().mockResolvedValue({ ok: true })
	}
	const callbacks = [vi.fn(), vi.fn(), vi.fn(), vi.fn()] as const
	return {
		create,
		read,
		api,
		callbacks,
		start: (value = target) => startArtifact(api, value, parentOrigin, ...callbacks)
	}
}

describe('artifact session ownership', () => {
	it('uses the compiled API, keeps line navigation in the reference, and returns full original source', async () => {
		const test = setup()
		const run = test.start()
		expect(test.api.createArtifactPreview).toHaveBeenCalledExactlyOnceWith({ ...target, parentOrigin })
		test.create.resolve(session)
		test.read.resolve(source)
		await run.settled
		expect(test.callbacks[0]).toHaveBeenCalledWith(session)
		expect(test.api.artifactPreviewSource).toHaveBeenCalledExactlyOnceWith(session.id)
		expect(test.callbacks[1]).toHaveBeenCalledExactlyOnceWith(source)
		await run.dispose()
		await run.dispose()
		expect(test.api.closeArtifactPreview).toHaveBeenCalledExactlyOnceWith(session.id)
	})

	it('closes a late create after cleanup without publishing the session or fetching source', async () => {
		const test = setup()
		const run = test.start()
		const cleanup = run.dispose()
		test.create.resolve(session)
		await cleanup
		expect(test.api.closeArtifactPreview).toHaveBeenCalledExactlyOnceWith(session.id)
		expect(test.api.artifactPreviewSource).not.toHaveBeenCalled()
		for (const callback of test.callbacks) expect(callback).not.toHaveBeenCalled()
	})

	it('isolates StrictMode cleanup/restart and refresh or file switching with out-of-order results', async () => {
		const old = setup()
		const active = setup()
		const first = old.start()
		const cleanup = first.dispose()
		const second = active.start({ kind: 'html', html: '<!DOCTYPE html>\n<p>exact</p>\n', baseDirectory: '/work' })
		active.create.resolve({ ...session, id: 'two' })
		active.read.resolve({ path: null, content: '<!DOCTYPE html>\n<p>exact</p>\n' })
		await second.settled
		old.create.resolve(session)
		await cleanup
		expect(old.callbacks[0]).not.toHaveBeenCalled()
		expect(old.api.closeArtifactPreview).toHaveBeenCalledExactlyOnceWith('one')
		expect(active.api.closeArtifactPreview).not.toHaveBeenCalled()
		expect(active.api.createArtifactPreview).toHaveBeenCalledWith({
			kind: 'html',
			html: '<!DOCTYPE html>\n<p>exact</p>\n',
			baseDirectory: '/work',
			parentOrigin
		})
		await second.dispose()
		expect(active.api.closeArtifactPreview).toHaveBeenCalledExactlyOnceWith('two')
	})

	it.each([false, true])('ignores late source completion after close (reject: %s)', async reject => {
		const test = setup()
		const run = test.start()
		test.create.resolve(session)
		await Promise.resolve()
		await run.dispose()
		if (reject) test.read.reject(new Error('stale read'))
		else test.read.resolve(source)
		await run.settled
		expect(test.callbacks[1]).not.toHaveBeenCalled()
		expect(test.callbacks[3]).not.toHaveBeenCalled()
	})

	it('shows create/source errors separately and contains failed cleanup requests', async () => {
		const creation = setup()
		const failed = creation.start()
		creation.create.reject(new Error('Preview unavailable'))
		await failed.settled
		expect(creation.callbacks[2]).toHaveBeenCalledWith('Preview unavailable')
		await failed.dispose()
		const reading = setup()
		reading.api.closeArtifactPreview.mockRejectedValue(new Error('offline'))
		const run = reading.start()
		reading.create.resolve(session)
		reading.read.reject(new Error('Source unavailable'))
		await run.settled
		expect(reading.callbacks[3]).toHaveBeenCalledWith('Source unavailable')
		expect(reading.callbacks[2]).not.toHaveBeenCalled()
		await expect(run.dispose()).resolves.toBeUndefined()
	})

	it('revokes a bad same-origin session before exposing its iframe URL', async () => {
		const test = setup()
		const run = test.start()
		test.create.resolve({ ...session, origin: parentOrigin })
		await run.settled
		expect(test.api.closeArtifactPreview).toHaveBeenCalledExactlyOnceWith(session.id)
		expect(test.callbacks[0]).not.toHaveBeenCalled()
		expect(test.api.artifactPreviewSource).not.toHaveBeenCalled()
		expect(test.callbacks[2]).toHaveBeenCalledWith(expect.stringContaining('invalid isolated origin'))
		await run.dispose()
	})
})
