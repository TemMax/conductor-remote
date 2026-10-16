import { describe, expect, it } from 'vitest'
import {
	ARTIFACT_SANDBOX,
	appendDiagnostic,
	artifactHandshake,
	artifactMessage,
	isolatedSession,
	MAX_DIAGNOSTICS,
	MAX_MESSAGE,
	MAX_SOURCE
} from '../../src/components/artifacts/protocol.ts'
import type { ArtifactPreviewSession } from '../../src/lib/types.ts'

const session: ArtifactPreviewSession = {
	id: 'preview-one',
	origin: 'http://localhost:31001',
	bootstrapUrl: 'http://localhost:31001/__conductor_preview/bootstrap?sessionId=preview-one',
	entryPath: '/index.html',
	grant: 'synthetic-grant',
	expiresAt: '2099-01-01T00:00:00Z',
	path: null
}
const frame = {} as MessageEventSource
const envelope = { channel: 'conductor-artifact-preview', version: 1, sessionId: session.id } as const
const parse = (data: unknown, origin = session.origin, source: MessageEventSource | null = frame) =>
	artifactMessage({ data, origin, source }, session, frame)

describe('artifact display protocol', () => {
	it('requires the exact origin, iframe window, version, channel and session', () => {
		expect(parse({ ...envelope, type: 'ready' })).toEqual({ ...envelope, type: 'ready' })
		for (const origin of [
			'null',
			'http://localhost:31002',
			'http://localhost:31001.evil.test',
			'http://127.0.0.1:31001'
		]) {
			expect(parse({ ...envelope, type: 'ready' }, origin)).toBeNull()
		}
		expect(parse({ ...envelope, type: 'ready' }, session.origin, {} as MessageEventSource)).toBeNull()
		expect(parse({ ...envelope, type: 'ready' }, session.origin, null)).toBeNull()
		expect(artifactMessage({ data: envelope, origin: session.origin, source: null }, session, null)).toBeNull()
		for (const bad of [{ version: 2 }, { channel: 'other' }, { sessionId: 'stale' }]) {
			expect(parse({ ...envelope, type: 'loaded', ...bad })).toBeNull()
		}
	})

	it('rejects commands and authorize messages, even from the right window', () => {
		for (const data of [
			null,
			'ready',
			1,
			{},
			[],
			{ ...envelope, type: 'authorize', grant: 'untrusted' },
			{ ...envelope, type: 'refresh' },
			{ ...envelope, type: 'close' },
			{ ...envelope, type: 'navigate', url: '/api' }
		]) {
			expect(parse(data)).toBeNull()
		}
	})

	it('validates and bounds untrusted diagnostics while stripping surplus fields', () => {
		for (const kind of ['javascript', 'promise', 'resource', 'http']) {
			const result = parse({
				...envelope,
				type: 'diagnostic',
				kind,
				message: 'm'.repeat(3000),
				source: 's'.repeat(3000),
				line: 12,
				column: 0,
				command: 'close'
			})
			expect(result).toEqual({
				...envelope,
				type: 'diagnostic',
				kind,
				message: 'm'.repeat(MAX_MESSAGE),
				source: 's'.repeat(MAX_SOURCE),
				line: 12,
				column: 0
			})
		}
		for (const bad of [
			{ kind: 'command' },
			{ kind: { toString: null } },
			{ message: {} },
			{ source: 4 },
			{ line: -1 },
			{ line: 1.5 },
			{ column: Infinity }
		]) {
			expect(parse({ ...envelope, type: 'diagnostic', kind: 'http', message: 'missing', ...bad })).toBeNull()
		}
		let diagnostics: Parameters<typeof appendDiagnostic>[0] = []
		for (let index = 0; index < MAX_DIAGNOSTICS + 20; index++)
			diagnostics = appendDiagnostic(diagnostics, { kind: 'http', message: String(index) })
		expect(diagnostics).toHaveLength(MAX_DIAGNOSTICS)
		expect(diagnostics[0].message).toBe('0')
	})

	it('authorizes once, ignores premature display messages, and never replays the grant', () => {
		const handshake = artifactHandshake()
		expect(handshake({ ...envelope, type: 'loaded' })).toBeNull()
		expect(handshake({ ...envelope, type: 'diagnostic', kind: 'http', message: 'early' })).toBeNull()
		expect(handshake({ ...envelope, type: 'ready' })).toBe('authorize')
		expect(handshake({ ...envelope, type: 'ready' })).toBeNull()
		expect(handshake({ ...envelope, type: 'loaded' })).toBe('loaded')
		expect(handshake({ ...envelope, type: 'ready' })).toBeNull()
		expect(handshake({ ...envelope, type: 'diagnostic', kind: 'http', message: 'missing' })).toBe('diagnostic')
	})

	it('requires a distinct HTTP(S) bootstrap origin and a minimal sandbox', () => {
		expect(isolatedSession(session, 'http://localhost:5173')).toBe(true)
		expect(isolatedSession(session, session.origin)).toBe(false)
		for (const bad of [
			{ origin: 'null' },
			{ origin: `${session.origin}/` },
			{ bootstrapUrl: 'https://elsewhere.test/__conductor_preview/bootstrap' },
			{ bootstrapUrl: `${session.origin}/index.html` },
			{ bootstrapUrl: 'http://user:pass@localhost:31001/__conductor_preview/bootstrap' },
			{ grant: '' },
			{ expiresAt: 'invalid' }
		])
			expect(isolatedSession({ ...session, ...bad }, 'http://localhost:5173')).toBe(false)
		expect(ARTIFACT_SANDBOX.split(' ')).toEqual(['allow-scripts', 'allow-same-origin'])
	})
})
