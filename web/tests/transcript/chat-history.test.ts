import { describe, expect, test } from 'vitest'
import { conversationTabs, latestChat, previousChats } from '../../src/lib/transcript/history.ts'
import type { ChatHistoryLink, Session } from '../../src/lib/types.ts'

const session = (id: string, createdAt = '2026-10-02T10:49:59.966Z'): Session => ({
	id,
	status: 'idle',
	title: `Title for ${id}`,
	model: 'opus-5-5-1m',
	permission_mode: 'default',
	claude_effort_level: 'high',
	fast_mode: 0,
	agent_type: 'claude',
	context_used_percent: 72.683,
	unread_count: 0,
	created_at: createdAt,
	updated_at: createdAt,
	last_user_message_at: null,
	prompt_cache_ttl_ms: null,
	turn_started_at: null,
	background_tasks: []
})

const continuation = (previousSessionId: string): ChatHistoryLink => ({
	previousSessionId,
	title: 'Agentic UI Chat Design',
	createdAt: '2026-10-02T10:49:59.966Z'
})

describe('available conversation contexts', () => {
	test('keeps the original tab and selection when its continuation is closed', () => {
		const original = session('original')
		const sessions = [original]
		const links = { closed: continuation(original.id) }

		expect(conversationTabs(sessions, links)).toEqual([original])
		expect(latestChat(original.id, links, sessions)).toBe(original.id)
		expect(latestChat('closed', links, sessions)).toBe(original.id)
	})

	test.each([
		{ open: ['first', 'second', 'third'], expected: 'third' },
		{ open: ['first', 'second'], expected: 'second' },
		{ open: ['first', 'third'], expected: 'third' },
		{ open: ['first'], expected: 'first' },
		{ open: ['third'], expected: 'third' },
		{ open: [], expected: null }
	])('resolves a multi-context conversation with open=$open', ({ open, expected }) => {
		const sessions = open.map(id => session(id))
		const links = { second: continuation('first'), third: continuation('second') }

		expect(conversationTabs(sessions, links).map(chat => chat.id)).toEqual(expected ? [expected] : [])
		for (const id of ['first', 'second', 'third']) {
			expect(latestChat(id, links, sessions)).toBe(expected)
		}
	})

	test('keeps the conversation title and ordering when its latest open context gets the tab', () => {
		const first = session('first')
		const second = session('second', '2026-10-08T09:20:37.352Z')
		const unrelated = session('unrelated', '2026-10-05T12:00:00.000Z')
		const sessions = [unrelated, first, second]
		const links = { second: continuation(first.id), closed: continuation(second.id) }

		expect(conversationTabs(sessions, links)).toEqual([
			{ ...second, title: links.second.title, created_at: links.second.createdAt },
			unrelated
		])
		expect(latestChat(first.id, links, sessions)).toBe(second.id)
		expect(latestChat('closed', links, sessions)).toBe(second.id)
		expect(latestChat(unrelated.id, links, sessions)).toBe(unrelated.id)
	})

	test('returns no selection for a missing or unset chat', () => {
		const sessions = [session('unrelated')]
		expect(latestChat('missing', {}, sessions)).toBeNull()
		expect(latestChat(null, {}, sessions)).toBeNull()
		expect(conversationTabs(sessions, {})).toEqual(sessions)
	})

	test('retains closed intermediate contexts in transcript history', () => {
		const links = { second: continuation('first'), third: continuation('second') }
		expect(previousChats('third', links)).toEqual(['first', 'second'])
	})

	test('keeps available tabs reachable if history links contain a cycle', () => {
		const sessions = [session('first'), session('second')]
		const links = { first: continuation('second'), second: continuation('first') }
		expect(conversationTabs(sessions, links).map(chat => chat.id)).toEqual(['first', 'second'])
		expect(latestChat('first', links, sessions)).toBe('first')
		expect(latestChat('second', links, sessions)).toBe('second')
		expect(previousChats('first', links)).toEqual(['second'])
	})
})
