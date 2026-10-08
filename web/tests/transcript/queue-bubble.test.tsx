import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, test, vi } from 'vitest'

Object.defineProperty(globalThis, 'location', { configurable: true, value: { hash: '', pathname: '/', search: '' } })
Object.defineProperty(globalThis, 'localStorage', {
	configurable: true,
	value: { getItem: () => null, setItem: () => {}, removeItem: () => {} }
})
Object.defineProperty(globalThis, 'history', { configurable: true, value: { replaceState: () => {} } })

const { QueueBubble } = await import('../../src/components/transcript/QueueBubble.tsx')

describe('queue bubble', () => {
	test('QueueBubble keeps pending and failed actions presentational and distinct', () => {
		const pending = renderToStaticMarkup(
			<QueueBubble state="pending" label="Queued · exploration" meta="Opening child chat">
				Inspect this.
			</QueueBubble>
		)
		expect(pending).toContain('Queued · exploration')
		expect(pending).toContain('Opening child chat')

		const failed = renderToStaticMarkup(
			<QueueBubble
				state="failed"
				label="Queued · exploration"
				meta="Model missing"
				actions={[
					{ label: 'Edit roles', onClick: vi.fn(), primary: true },
					{ label: 'Dismiss', onClick: vi.fn() }
				]}
			>
				Inspect this.
			</QueueBubble>
		)
		expect(failed).toContain('Edit roles')
		expect(failed).toContain('Dismiss')
	})
})
