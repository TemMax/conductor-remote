import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, test } from 'vitest'
import type { WorkspaceActions } from '../../src/components/workspaces/WorkspaceMenu.tsx'
import type { Workspace } from '../../src/lib/types.ts'

// `WorkspaceMenu` reaches the app store, which reads the URL and storage as it loads.
Object.defineProperty(globalThis, 'location', { configurable: true, value: { hash: '', pathname: '/', search: '' } })
Object.defineProperty(globalThis, 'localStorage', {
	configurable: true,
	value: { getItem: () => null, setItem: () => {}, removeItem: () => {} }
})
Object.defineProperty(globalThis, 'history', { configurable: true, value: { replaceState: () => {} } })

const { WorkspaceMenu } = await import('../../src/components/workspaces/WorkspaceMenu.tsx')

const workspace = { id: 'w1', state: 'ready', session_status: 'idle', pr_status: null } as Workspace

const actions = (agentsRefused: boolean): WorkspaceActions => ({
	busy: null,
	error: null,
	agentsRefused,
	setStatus: async () => {},
	archive: async () => {},
	dismissError: () => {}
})

const render = (agentsRefused: boolean) =>
	renderToStaticMarkup(<WorkspaceMenu workspace={workspace} agentsRunning={0} actions={actions(agentsRefused)} />)

describe('archive confirmation after a refusal', () => {
	test('shows the stop-agents confirmation when the relay refused the archive', () => {
		const html = render(true)
		expect(html).toContain('Archive workspace?')
		expect(html).toContain('Agents are still running in this workspace')
		expect(html).toContain('Stop agents and archive')
	})

	test('shows none of it otherwise', () => {
		const html = render(false)
		expect(html).not.toContain('Archive workspace?')
		expect(html).not.toContain('Agents are still running in this workspace')
		expect(html).not.toContain('Stop agents and archive')
	})
})
