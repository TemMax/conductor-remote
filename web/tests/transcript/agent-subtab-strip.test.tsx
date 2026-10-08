import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, test, vi } from 'vitest'

Object.defineProperty(globalThis, 'location', { configurable: true, value: { hash: '', pathname: '/', search: '' } })
Object.defineProperty(globalThis, 'localStorage', {
	configurable: true,
	value: { getItem: () => null, setItem: () => {}, removeItem: () => {} }
})
Object.defineProperty(globalThis, 'history', { configurable: true, value: { replaceState: () => {} } })

const { AgentSubtabStrip } = await import('../../src/components/transcript/AgentSubtabStrip.tsx')

describe('agent subtab strip', () => {
	test.each([0, 1])('keeps both native subtabs visible with only child %s selected', selectedIndex => {
		const tabs = [
			{
				key: 'tool-call-1',
				label: 'Inspect parser',
				model: '5.6 Sol',
				agentType: 'codex',
				selected: selectedIndex === 0,
				onSelect: vi.fn()
			},
			{
				key: 'tool-call-2',
				label: 'Inspect rendering',
				model: '5.6 Terra',
				agentType: 'codex',
				selected: selectedIndex === 1,
				onSelect: vi.fn()
			}
		]
		const html = renderToStaticMarkup(<AgentSubtabStrip label="Subagents" tabs={tabs} />)

		expect(html).toContain('aria-label="Subagents"')
		expect(html).toContain('aria-current="page"')
		expect(html).toContain('Inspect parser')
		expect(html).toContain('5.6 Sol')
		expect(html).toContain('Inspect rendering')
		expect(html).toContain('5.6 Terra')
		expect(html.match(/<button/g)).toHaveLength(2)
		const selectedButtons = html.match(/<button\b[^>]*aria-current="page"[^>]*>[\s\S]*?<\/button>/g) ?? []
		expect(selectedButtons).toHaveLength(1)
		expect(selectedButtons[0]).toContain(tabs[selectedIndex].label)
		expect(selectedButtons[0]).toContain('bg-text text-bg')
		// Provider icons and metadata need contrasting ink on the inverted surface too.
		expect(selectedButtons[0]).not.toContain('color:var(--color-provider-openai)')
		expect(selectedButtons[0]).toContain('text-bg/75')
	})
})
