import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, test, vi } from 'vitest'
import { AgentControls } from '../../src/components/agents/AgentControls.tsx'

function fastButton(fast: boolean, fastStaged?: boolean): string {
	const html = renderToStaticMarkup(
		<AgentControls
			model="Opus 5.5"
			providerModel="opus-5-5"
			agentType="claude"
			models={[]}
			fast={fast}
			fastStaged={fastStaged}
			onModelChange={vi.fn()}
			onFastChange={vi.fn()}
			onEffortChange={vi.fn()}
			onPlanChange={vi.fn()}
		/>
	)
	const label = `Fast mode ${fast ? 'on' : 'off'}`
	const match = html.match(new RegExp(`<button[^>]*aria-label="${label}"[^>]*>.*?</button>`))
	expect(match).not.toBeNull()
	return match?.[0] ?? ''
}

describe('fast button indicator', () => {
	test('on: accent colour and filled icon', () => {
		const button = fastButton(true)
		expect(button).toContain('fill="currentColor"')
		expect(button).toContain('text-accent')
	})

	test('off: muted outline', () => {
		const button = fastButton(false)
		expect(button).toContain('fill="none"')
		expect(button).toContain('text-muted')
		expect(button).not.toContain('text-accent')
	})

	test('staged change to off: accent outline', () => {
		const button = fastButton(false, true)
		expect(button).toContain('fill="none"')
		expect(button).toContain('text-accent')
	})
})
