import { afterEach, expect, it, vi } from 'vitest'
import { ApiError, client } from '../src/lib/api.ts'

afterEach(() => vi.unstubAllGlobals())

it('preserves an uncertain submission so the card cannot send again', async () => {
	vi.stubGlobal('localStorage', { getItem: () => null })
	vi.stubGlobal(
		'fetch',
		vi
			.fn()
			.mockResolvedValue(new Response(JSON.stringify({ error: 'Check Conductor', submitted: true }), { status: 502 }))
	)
	const error = await client
		.answerQuestions('sample-chat', 'sample-ws', 'sample-question', [{ selected: [0] }])
		.catch(error => error)
	expect(error).toBeInstanceOf(ApiError)
	expect(error.submitted).toBe(true)
	expect(error.status).toBe(502)
})
