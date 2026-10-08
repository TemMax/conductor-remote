import type { MessagesResponse } from '../../src/contract/wire.ts'

/**
 * What `GET /api/sessions/msg-chat/messages` returns for the relay's seeded test database, read from the
 * start: a prompt, thinking, prose, a call with its result, a failing command, the closing answer, and the
 * four renderable prompts of the queue in sending order.
 *
 * The row after the last entry is the end of the turn, which has no entry of its own, so the cursor is
 * 9 while the last entry is from row 8. The relay's test produces the same value from the seeded rows and
 * compares it with `fixtures/messages.json`, which the test beside this file holds equal to this constant.
 */
export const messages: MessagesResponse = {
	entries: [
		{
			id: 'msg-u1',
			rowid: 1,
			role: 'user',
			text: 'Add a retry to the fetch helper.',
			ts: '2026-09-14T10:00:01.000Z',
			queued: false
		},
		{
			id: 'msg-a1:0',
			rowid: 3,
			role: 'thinking',
			text: 'Read the helper before changing it.',
			ts: '2026-09-14T10:00:03.000Z',
			queued: false
		},
		{
			id: 'msg-a1:1',
			rowid: 3,
			role: 'assistant',
			text: "I'll read the helper first.",
			ts: '2026-09-14T10:00:03.000Z',
			queued: false
		},
		{
			id: 'msg-a1:2',
			rowid: 3,
			role: 'tool',
			text: 'Read',
			tool: 'Read',
			detail: 'src/fetch.ts',
			toolUseId: 'msg-tool-1',
			ts: '2026-09-14T10:00:03.000Z',
			queued: false
		},
		{
			id: 'msg-r1:0',
			rowid: 4,
			role: 'tool',
			text: '',
			output: 'export async function fetchJson(url) { return fetch(url) }',
			toolUseId: 'msg-tool-1',
			ts: '2026-09-14T10:00:04.000Z',
			queued: false
		},
		{
			id: 'msg-a2:0',
			rowid: 5,
			role: 'tool',
			text: 'Run the tests',
			tool: 'Bash',
			detail: 'npm test',
			toolUseId: 'msg-tool-2',
			ts: '2026-09-14T10:00:05.000Z',
			queued: false
		},
		{
			id: 'msg-r2:0',
			rowid: 6,
			role: 'tool',
			text: '1 test failed: fetchJson retries',
			output: '1 test failed: fetchJson retries',
			toolUseId: 'msg-tool-2',
			error: true,
			ts: '2026-09-14T10:00:06.000Z',
			queued: false
		},
		{
			id: 'msg-a3:0',
			rowid: 8,
			role: 'assistant',
			text: 'The retry is in; the failing test needs a longer timeout.',
			ts: '2026-09-14T10:00:08.000Z',
			queued: false
		}
	],
	cursor: 9,
	queued: [
		{
			id: 'msg-q-first',
			rowid: 0,
			role: 'user',
			text: 'Run the linter.',
			ts: '2026-09-14T10:05:03.000Z',
			queued: true
		},
		{
			id: 'msg-q-second',
			rowid: 0,
			role: 'user',
			text: 'Then update the docs.',
			ts: '2026-09-14T10:05:02.000Z',
			queued: true
		},
		{
			id: 'msg-q-second-later',
			rowid: 0,
			role: 'user',
			text: 'And bump the version.',
			ts: '2026-09-14T10:05:05.000Z',
			queued: true
		},
		{
			id: 'msg-q-unnumbered',
			rowid: 0,
			role: 'user',
			text: 'Last in line.',
			ts: '2026-09-14T10:05:01.000Z',
			queued: true
		}
	]
}
