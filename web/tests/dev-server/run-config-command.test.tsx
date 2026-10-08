/**
 * The Run menu's row (web/src/components/workspaces/DevServerControls.tsx ▸ RunConfigRow).
 *
 * The row shows the command Conductor runs under the task's name when the host reported one,
 * with the full command as its `title` since the line is truncated; without a command there is
 * no empty line under the name.
 *
 * The browser globals are stubbed as in run-badge.test.tsx: the component's module imports the
 * app's store, which reads the URL and localStorage at load.
 */

import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'

Object.defineProperty(globalThis, 'location', { configurable: true, value: { hash: '', pathname: '/', search: '' } })
Object.defineProperty(globalThis, 'localStorage', {
	configurable: true,
	value: { getItem: () => null, setItem: () => {}, removeItem: () => {} }
})
Object.defineProperty(globalThis, 'history', { configurable: true, value: { replaceState: () => {} } })

const { RunConfigRow } = await import('../../src/components/workspaces/DevServerControls.tsx')

const rowFor = (config: { id: string; name: string; command?: string }) =>
	renderToStaticMarkup(<RunConfigRow config={config} onSelect={() => {}} />)

describe('RunConfigRow', () => {
	it('shows the command under the name, with the full command as its title', () => {
		const html = rowFor({ id: 'dev', name: 'Dev', command: 'npm run dev -- --port 3000' })
		expect(html).toContain('Dev')
		expect(html).toContain('>npm run dev -- --port 3000</span>')
		expect(html).toContain('title="npm run dev -- --port 3000"')
		expect(html).toContain('font-mono')
	})

	it('renders no command line when the config has none', () => {
		const html = rowFor({ id: 'dev', name: 'Dev' })
		expect(html).toContain('Dev')
		expect(html).not.toContain('font-mono')
		expect(html).not.toContain('title=')
	})
})
