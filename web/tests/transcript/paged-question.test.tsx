import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { renderToStaticMarkup } from 'react-dom/server'
import { expect, it, vi } from 'vitest'

vi.mock('../../src/hooks/send.ts', () => ({ useSendPrompt: () => vi.fn() }))
vi.mock('../../src/hooks/transcript.ts', () => ({
	useTranscript: () => ({
		entries: [],
		loading: false,
		error: null,
		hasOlder: true,
		loadingOlder: false,
		olderError: null,
		loadOlder: vi.fn(),
		pendingQuestion: {
			id: 'synthetic-question',
			provider: 'claude',
			questions: [
				{
					header: null,
					question: 'Choose a synthetic colour',
					multiSelect: false,
					options: [{ label: 'Blue', description: '' }]
				}
			]
		}
	})
}))
Object.defineProperty(globalThis, 'location', { configurable: true, value: { hash: '', pathname: '/', search: '' } })
Object.defineProperty(globalThis, 'localStorage', {
	configurable: true,
	value: { getItem: () => null, setItem: () => {}, removeItem: () => {} }
})
Object.defineProperty(globalThis, 'history', { configurable: true, value: { replaceState: () => {} } })
const { Transcript } = await import('../../src/components/transcript/Transcript.tsx')

it('shows an older active question even when the latest source rows have no visible entries', () => {
	const html = renderToStaticMarkup(
		<QueryClientProvider client={new QueryClient()}>
			<Transcript sessionId="synthetic-session" workspaceId="synthetic-workspace" working={false} />
		</QueryClientProvider>
	)
	expect(html).toContain('Choose a synthetic colour')
	expect(html).toContain('Blue')
	expect(html).not.toContain('No messages yet')
})
