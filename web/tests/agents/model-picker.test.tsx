import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, test, vi } from 'vitest'
import { ModelPicker } from '../../src/components/agents/ModelPicker.tsx'

describe('model picker default', () => {
	test('Auto comes before concrete models and offers its own settings', () => {
		const html = renderToStaticMarkup(
			<ModelPicker
				open
				models={['5.6 Luna']}
				autoSelected
				onSelectAuto={vi.fn()}
				onAutoSettings={vi.fn()}
				onSelect={vi.fn()}
			/>
		)
		expect(html.indexOf('>Auto</span>')).toBeLessThan(html.indexOf('>5.6 Luna</span>'))
		expect(html).toContain('aria-label="Auto model settings"')
		const ordinary = renderToStaticMarkup(<ModelPicker open models={['5.6 Luna']} onSelect={vi.fn()} />)
		expect(ordinary).not.toContain('>Auto</span>')
	})
	test('uses a wider menu and shortens OpenCode Go labels without changing their values', () => {
		const html = renderToStaticMarkup(<ModelPicker open models={['opencode-go/grok-4.6']} onSelect={vi.fn()} />)
		expect(html).toContain('max-h-64 w-64')
		expect(html).toContain('>go/grok-4.6</span>')
	})

	test('offers no default-model action', () => {
		const html = renderToStaticMarkup(
			<ModelPicker open value="5.6 Terra" models={['5.6 Sol', '5.6 Terra']} onSelect={vi.fn()} />
		)
		expect(html).not.toContain('as default')
		expect(html).not.toContain('is the default model')
	})
})
