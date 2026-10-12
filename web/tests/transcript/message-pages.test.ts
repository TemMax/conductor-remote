import { describe, expect, it } from 'vitest'
import { groupSteps } from '../../src/components/transcript/grouping.ts'
import {
	currentCopyEntries,
	foldMessagePages,
	mergeEntries,
	mergeMessagePages
} from '../../src/lib/transcript/merge.ts'
import type { TranscriptEntry } from '../../src/lib/types.ts'

const call: TranscriptEntry = {
	id: 'call',
	rowid: 2,
	role: 'tool',
	tool: 'Bash',
	toolUseId: 'tool-1',
	text: 'Bash',
	ts: '2026-01-01',
	queued: false
}
const result: TranscriptEntry = {
	id: 'result',
	rowid: 5,
	role: 'tool',
	toolUseId: 'tool-1',
	text: '',
	output: 'synthetic output',
	ts: '2026-01-01',
	queued: false
}

describe('transcript pages arriving newest first', () => {
	it('keeps an unmatched success until its older call arrives', () => {
		const raw = mergeMessagePages([], [result])
		expect(raw).toEqual([result])
		const backfilled = mergeMessagePages(raw, [call])
		expect(backfilled).toEqual([call, result])
		expect(mergeEntries([], backfilled)).toEqual([
			{ ...call, output: 'synthetic output', diff: undefined, images: undefined, error: undefined }
		])
	})

	it('deduplicates context calls when their real history page arrives', () => {
		const raw = mergeMessagePages([call, result], [{ ...call }])
		expect(raw).toEqual([call, result])
		expect(raw[0]).toBe(call)
	})

	it('retains the parser order of multiple entries from one source row', () => {
		const early = { ...call, id: 'early:10', rowid: 1 }
		const first = { ...call, id: 'recent:2', rowid: 3 }
		const second = { ...call, id: 'recent:10', rowid: 3 }
		expect(mergeMessagePages([first, second], [early])).toEqual([early, first, second])
	})

	it('preserves the raw buffer identity on an empty poll', () => {
		const raw = [call, result]
		expect(mergeMessagePages(raw, [])).toBe(raw)
	})

	it('restores earlier parts of a frame before its previously fetched context call', () => {
		const tool = { ...call, id: 'frame:2' }
		const prose = { ...call, id: 'frame:0', role: 'assistant' as const }
		const thought = { ...call, id: 'frame:1', role: 'thinking' as const }
		expect(mergeMessagePages([tool, result], [prose, thought, tool]).map(e => e.id)).toEqual([
			'frame:0',
			'frame:1',
			'frame:2',
			'result'
		])
	})

	it('keeps a singleton step mounted when older adjacent steps arrive', () => {
		const visible = { e: call, children: [] }
		const previous = groupSteps([visible])
		const older = { e: { ...call, id: 'older', rowid: 1 }, children: [] }
		const next = groupSteps([older, visible], previous)
		expect(previous[0].kind).toBe('steps')
		expect(next[0].key).toBe(previous[0].key)
	})

	it('preserves an unchanged folded call while another page arrives', () => {
		const visible = foldMessagePages([], [call, result])
		const older = { ...call, id: 'older', rowid: 1, toolUseId: 'older-tool' }
		const next = foldMessagePages(visible, [older, call, result])
		expect(next.find(e => e.id === call.id)).toBe(visible[0])
	})

	it('keeps a step disclosure mounted when an older page extends its group', () => {
		const old = { e: call, children: [] }
		const latest = { e: { ...call, id: 'latest', rowid: 4 }, children: [] }
		const previous = groupSteps([old, latest])
		const earlier = { e: { ...call, id: 'earlier', rowid: 1 }, children: [] }
		const next = groupSteps([earlier, old, latest], previous)
		expect(next[0].key).toBe(previous[0].key)
	})

	it('copies a new tool result and later response after full-history preloading', () => {
		const older = { ...call, id: 'older', rowid: 1 }
		const updated = { ...call, output: 'new result' }
		const response = { ...call, id: 'response', rowid: 6, role: 'assistant' as const }
		expect(currentCopyEntries([older, call], [updated, response])).toEqual([older, updated, response])
	})

	it('keeps a result loaded ahead of the visible polling window', () => {
		const complete = { ...call, output: 'preloaded result' }
		expect(currentCopyEntries([complete], [call])).toEqual([complete])
	})
})
