import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, test } from 'vitest'
import type { StateResponse } from '../../src/contract/wire.ts'

// `ui.tsx` reaches the app store, which reads the URL and storage as it loads.
Object.defineProperty(globalThis, 'location', { configurable: true, value: { hash: '', pathname: '/', search: '' } })
Object.defineProperty(globalThis, 'localStorage', {
	configurable: true,
	value: { getItem: () => null, setItem: () => {}, removeItem: () => {} }
})
Object.defineProperty(globalThis, 'history', { configurable: true, value: { replaceState: () => {} } })

const { ConductorOffline, isConductorOffline } = await import('../../src/components/workspaces/ConductorOffline.tsx')

const state = (extra: Partial<StateResponse> = {}): StateResponse => ({
	workspaces: [],
	actuator: {} as StateResponse['actuator'],
	...extra
})

describe('isConductorOffline', () => {
	test('is false before the first state arrives', () => {
		expect(isConductorOffline(undefined)).toBe(false)
	})

	test('treats a missing field as running, as an older relay sends it', () => {
		expect(isConductorOffline(state())).toBe(false)
	})

	test('is false while Conductor is running', () => {
		expect(isConductorOffline(state({ conductor: { running: true } }))).toBe(false)
	})

	test('is true only when the relay says it is not running', () => {
		expect(isConductorOffline(state({ conductor: { running: false } }))).toBe(true)
	})
})

describe('ConductorOffline', () => {
	test('names the state and offers the launch button', () => {
		const html = renderToStaticMarkup(<ConductorOffline onLaunch={() => {}} pending={false} error={null} />)
		expect(html).toContain('Conductor is not running')
		expect(html).toContain('Open it on your Mac to see your workspaces.')
		expect(html).toContain('Launch Conductor')
		expect(html).not.toContain('disabled=""')
	})

	test('disables the button and says so while a launch is pending', () => {
		const html = renderToStaticMarkup(<ConductorOffline onLaunch={() => {}} pending error={null} />)
		expect(html).toContain('disabled=""')
		expect(html).toContain('Launching…')
		expect(html).not.toContain('Launch Conductor')
	})

	test('shows an error under the button', () => {
		const html = renderToStaticMarkup(
			<ConductorOffline onLaunch={() => {}} pending={false} error="Could not open it" />
		)
		expect(html).toContain('Could not open it')
		expect(html.indexOf('Could not open it')).toBeGreaterThan(html.indexOf('</button>'))
	})
})
