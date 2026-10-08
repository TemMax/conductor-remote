import { describe, expect, it } from 'vitest'
import { groupSteps } from '../src/components/transcript/grouping.ts'
import { mergeEntries } from '../src/lib/transcript/merge.ts'
import type { TranscriptEntry } from '../src/lib/types.ts'

const entry = (rowid: number): TranscriptEntry => ({
	id: 'question:codex:sample',
	rowid,
	role: 'assistant',
	text: 'Choose colour',
	ts: '',
	queued: false,
	question: {
		id: 'sample',
		provider: 'codex',
		questions: [
			{ header: null, question: 'Choose colour', multiSelect: false, options: [{ label: 'Red', description: '' }] }
		]
	}
})

describe('interactive questions in the transcript', () => {
	it('deduplicates provider replays even across incremental polls', () => {
		const first = mergeEntries([], [entry(1), entry(2)])
		expect(first).toHaveLength(1)
		expect(mergeEntries(first, [entry(3)])).toHaveLength(1)
	})
	it('keeps question cards visible outside collapsed tool steps', () => {
		const question = { ...entry(2), role: 'tool' as const }
		const step = { ...entry(1), id: 'step', question: undefined, role: 'tool' as const }
		expect(
			groupSteps([
				{ e: step, children: [] },
				{ e: question, children: [] }
			]).map(r => r.kind)
		).toEqual(['entry', 'entry'])
	})
})

import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { QuestionCard } from '../src/components/transcript/QuestionCard.tsx'

it('historical question cards have no submit button and their options are disabled', () => {
	const markup = renderToStaticMarkup(
		createElement(QuestionCard, {
			request: entry(1).question!,
			sessionId: 'sample-chat',
			workspaceId: 'sample-ws',
			active: false
		})
	)
	expect(markup).not.toContain('<button')
	expect(markup).toContain('disabled=""')
})

it('multiple questions use one shared submit button', () => {
	const request = entry(1).question!
	request.questions.push({ ...request.questions[0], question: 'Choose shape' })
	const markup = renderToStaticMarkup(
		createElement(QuestionCard, { request, sessionId: 'sample-chat', workspaceId: 'sample-ws', active: true })
	)
	expect(markup.match(/<fieldset/g)).toHaveLength(2)
	expect(markup.match(/<button/g)).toHaveLength(1)
})
