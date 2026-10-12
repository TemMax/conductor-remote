import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'
import type { TranscriptEntry } from '../../src/lib/types.ts'

Object.defineProperty(globalThis, 'location', { configurable: true, value: { hash: '', pathname: '/', search: '' } })
Object.defineProperty(globalThis, 'localStorage', {
	configurable: true,
	value: { getItem: () => null, setItem: () => {}, removeItem: () => {} }
})
Object.defineProperty(globalThis, 'history', { configurable: true, value: { replaceState: () => {} } })

const { NodeEntry, StepGroup } = await import('../../src/components/transcript/entries.tsx')
const { ToolEntry } = await import('../../src/components/transcript/ToolEntry.tsx')
const entry = (overrides: Partial<TranscriptEntry>): TranscriptEntry => ({
	id: 'synthetic-step',
	rowid: 1,
	role: 'tool',
	text: 'Bash',
	ts: '2026-01-01T00:00:00Z',
	queued: false,
	...overrides
})

describe('collapsed transcript steps', () => {
	it('keeps the group count and failures without creating the hidden step bodies', () => {
		const html = renderToStaticMarkup(
			<StepGroup
				nodes={[
					{ e: entry({ detail: 'echo synthetic-input', output: 'synthetic-output', error: true }), children: [] },
					{ e: entry({ id: 'thinking-step', role: 'thinking', text: 'synthetic-thinking' }), children: [] }
				]}
			/>
		)
		expect(html).toContain('2 steps')
		expect(html).toContain('1 failed')
		expect(html).not.toContain('synthetic-output')
		expect(html).not.toContain('synthetic-thinking')
	})

	it('does not render a closed tool input or output as code', () => {
		const html = renderToStaticMarkup(
			<ToolEntry e={entry({ detail: 'echo synthetic-input', output: 'synthetic-output' })} />
		)
		expect(html).toContain('Bash')
		expect(html).not.toContain('<pre')
		expect(html).not.toContain('synthetic-output')
	})

	it('keeps the thinking disclosure without parsing its hidden markdown', () => {
		const html = renderToStaticMarkup(
			<NodeEntry node={{ e: entry({ role: 'thinking', text: 'synthetic-hidden-thought' }), children: [] }} />
		)
		expect(html).toContain('Thinking')
		expect(html).not.toContain('synthetic-hidden-thought')
	})
})
