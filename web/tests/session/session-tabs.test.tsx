import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, test, vi } from 'vitest'
import type { Session } from '../../src/contract/wire.ts'

Object.defineProperty(globalThis, 'location', { configurable: true, value: { hash: '', pathname: '/', search: '' } })
Object.defineProperty(globalThis, 'localStorage', {
	configurable: true,
	value: { getItem: () => null, setItem: () => {}, removeItem: () => {} }
})
Object.defineProperty(globalThis, 'history', { configurable: true, value: { replaceState: () => {} } })

const { DiffButton, DiffFileScopeToggle, DiffFolderToggle } = await import('../../src/components/session/DiffPanel.tsx')
const { SessionTabs } = await import('../../src/components/session/SessionTabs.tsx')
const { SubagentReplyNotice } = await import('../../src/components/session/SessionNotices.tsx')
const { ClosedTabsList } = await import('../../src/components/session/ClosedTabsSheet.tsx')

const session: Session = {
	id: 'chat-1',
	status: 'idle',
	title: 'Alpha',
	model: 'Sonnet',
	permission_mode: 'default',
	claude_effort_level: 'high',
	fast_mode: 0,
	agent_type: 'claude',
	context_used_percent: 42,
	unread_count: 0,
	created_at: '2026-09-03 10:00:00',
	updated_at: '2026-09-03 10:00:00',
	last_user_message_at: null,
	prompt_cache_ttl_ms: null,
	turn_started_at: null,
	background_tasks: []
}

describe('phone chat tabs', () => {
	test('hides the close control when there is only one tab', () => {
		const html = renderToStaticMarkup(
			<SessionTabs
				sessions={[session]}
				activeId={session.id}
				readMarks={{}}
				promptStates={{}}
				onSelect={vi.fn()}
				onContext={vi.fn()}
				onNewChat={vi.fn()}
				onClose={vi.fn()}
				onClosedTabs={vi.fn()}
				creating={false}
				closingId={null}
				online
			/>
		)

		expect(html).toContain('Alpha')
		expect(html).not.toContain('aria-label="Close Alpha chat"')
		expect(html).toContain('aria-label="Context for Alpha: 42% used"')
		expect(html.match(/<button/g)).toHaveLength(4)
	})

	test('keeps selection and close as separate controls with multiple tabs', () => {
		const secondSession: Session = { ...session, id: 'chat-2', title: 'Beta' }
		const html = renderToStaticMarkup(
			<SessionTabs
				sessions={[session, secondSession]}
				activeId={session.id}
				readMarks={{}}
				promptStates={{}}
				onSelect={vi.fn()}
				onContext={vi.fn()}
				onNewChat={vi.fn()}
				onClose={vi.fn()}
				onClosedTabs={vi.fn()}
				creating={false}
				closingId={null}
				online
			/>
		)

		expect(html).toContain('aria-label="Close Alpha chat"')
		expect(html).toContain('aria-label="Close Beta chat"')
		expect(html.match(/<button/g)).toHaveLength(8)
	})

	test.each([true, false])('keeps one selected tab with the file active=%s', active => {
		const html = renderToStaticMarkup(
			<SessionTabs
				sessions={[session]}
				activeId={session.id}
				readMarks={{}}
				promptStates={{}}
				fileTab={{ path: 'web/src/App.tsx', active, onSelect: vi.fn(), onClose: vi.fn() }}
				onSelect={vi.fn()}
				onContext={vi.fn()}
				onNewChat={vi.fn()}
				onClose={vi.fn()}
				onClosedTabs={vi.fn()}
				creating={false}
				closingId={null}
				online={false}
			/>
		)

		const currentButton = html.match(/<button\b[^>]*aria-current="page"[^>]*>[\s\S]*?<\/button>/g) ?? []
		expect(currentButton).toHaveLength(1)
		expect(currentButton[0]).toContain(active ? 'App.tsx' : 'Alpha')
		// Switching to the chat keeps the local file reachable, with the full path
		// available even though its visible label is only the filename.
		expect(html).toContain('aria-label="Open web/src/App.tsx"')
		expect(html).toContain('title="web/src/App.tsx"')
		expect(html).toContain('<span class="whitespace-nowrap">App.tsx</span>')
		// The preview can close offline and does not count as a second Conductor chat.
		expect(html).toMatch(/<button\b(?![^>]*disabled)[^>]*aria-label="Close web\/src\/App.tsx file tab"/)
		expect(html).not.toContain('aria-label="Close Alpha chat"')
	})

	test('shows the full tab title without a width cap', () => {
		const longTitle = 'Auk brain memory retrieval improvements'
		const html = renderToStaticMarkup(
			<SessionTabs
				sessions={[{ ...session, title: longTitle }]}
				activeId={session.id}
				readMarks={{}}
				promptStates={{}}
				onSelect={vi.fn()}
				onContext={vi.fn()}
				onNewChat={vi.fn()}
				onClose={vi.fn()}
				onClosedTabs={vi.fn()}
				creating={false}
				closingId={null}
				online
			/>
		)

		expect(html).toContain(`<span class="whitespace-nowrap">${longTitle}</span>`)
		expect(html).not.toContain('truncate')
		expect(html).not.toContain('max-w-36')
	})

	test('keeps New chat and Closed tabs reachable after the last tab closes', () => {
		const html = renderToStaticMarkup(
			<SessionTabs
				sessions={[]}
				activeId={null}
				readMarks={{}}
				promptStates={{}}
				onSelect={vi.fn()}
				onContext={vi.fn()}
				onNewChat={vi.fn()}
				onClose={vi.fn()}
				onClosedTabs={vi.fn()}
				creating={false}
				closingId={null}
				online
			/>
		)
		expect(html).toContain('aria-label="New chat, same files"')
		expect(html).toContain('aria-label="Closed tabs"')
	})

	test('makes a native child read-only and names the parent reply destination', () => {
		const html = renderToStaticMarkup(<SubagentReplyNotice title="Alpha" onReturn={vi.fn()} />)

		expect(html).toContain('Return to Alpha to reply')
		expect(html).toContain('border-t')
	})
})

describe('closed tab picker', () => {
	test('finds closed chats by title and model and retains a restore action for duplicate titles', () => {
		const sessions = [session, { ...session, id: 'chat-2', model: 'gpt-5.6-sol' }]
		const html = renderToStaticMarkup(
			<ClosedTabsList sessions={sessions} filter="alpha sol" restoringId={null} online onRestore={vi.fn()} />
		)
		expect(html).toContain('aria-label="Restore Alpha chat"')
		expect(html.match(/<button/g)).toHaveLength(1)
		const duplicates = renderToStaticMarkup(
			<ClosedTabsList sessions={sessions} filter="" restoringId={null} online onRestore={vi.fn()} />
		)
		expect(duplicates.match(/aria-label="Restore Alpha chat"/g)).toHaveLength(2)
	})

	test('distinguishes an empty history from an unmatched search', () => {
		const empty = renderToStaticMarkup(
			<ClosedTabsList sessions={[]} filter="" restoringId={null} online onRestore={vi.fn()} />
		)
		const unmatched = renderToStaticMarkup(
			<ClosedTabsList sessions={[session]} filter="missing" restoringId={null} online onRestore={vi.fn()} />
		)
		expect(empty).toContain('No closed tabs in this workspace.')
		expect(unmatched).toContain('No closed tabs match your search.')
	})

	test('disables restores offline and prevents a second restore while one is waiting', () => {
		const offline = renderToStaticMarkup(
			<ClosedTabsList sessions={[session]} filter="" restoringId={null} online={false} onRestore={vi.fn()} />
		)
		const restoring = renderToStaticMarkup(
			<ClosedTabsList
				sessions={[session, { ...session, id: 'chat-2', title: 'Beta' }]}
				filter=""
				restoringId={session.id}
				online
				onRestore={vi.fn()}
			/>
		)
		expect(offline).toContain('disabled=""')
		expect(restoring.match(/disabled=""/g)).toHaveLength(2)
		expect(restoring.match(/Restoring…/g)).toHaveLength(1)
	})
})

describe('workspace diff shortcut', () => {
	test('offers changed and all file scopes', () => {
		const changed = renderToStaticMarkup(<DiffFileScopeToggle scope="changed" onChange={vi.fn()} />)
		const all = renderToStaticMarkup(<DiffFileScopeToggle scope="all" onChange={vi.fn()} />)

		expect(changed).toContain('aria-label="Changed files" aria-pressed="true"')
		expect(changed).toContain('aria-label="All files" aria-pressed="false"')
		expect(all).toContain('aria-label="Changed files" aria-pressed="false"')
		expect(all).toContain('aria-label="All files" aria-pressed="true"')
	})

	test('makes folder grouping an explicit file-rail preference', () => {
		const folders = renderToStaticMarkup(<DiffFolderToggle showFolders onChange={vi.fn()} />)
		const flat = renderToStaticMarkup(<DiffFolderToggle showFolders={false} onChange={vi.fn()} />)

		expect(folders).toContain('aria-label="Group files into folders"')
		expect(folders).toContain('aria-pressed="true"')
		expect(flat).toContain('aria-pressed="false"')
	})

	test('shows a dot only when the workspace has changes', () => {
		const changed = renderToStaticMarkup(
			<DiffButton stats={{ added: 12, removed: 0 }} open={false} onToggle={vi.fn()} />
		)
		const clean = renderToStaticMarkup(<DiffButton stats={{ added: 0, removed: 0 }} open={false} onToggle={vi.fn()} />)

		expect(changed).toContain('Toggle diff panel, changes available')
		expect(changed).toContain('bg-accent')
		expect(clean).toContain('aria-label="Toggle diff panel"')
		expect(clean).not.toContain('bg-accent')
	})
})
