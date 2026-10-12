import { isToolResult } from '../../contract/shared.ts'
import type { TranscriptEntry } from '../types.ts'

// Parser ids append the numeric part index to the source frame id. Context pages
// can supply a later part before the full frame arrives; insertion order is then wrong.
const partIndex = (id: string) => Number(/:(\d+)$/.exec(id)?.[1] ?? 0)

/** Keep raw results until an older page supplies their call; folding happens afterwards. */
export function mergeMessagePages(prev: TranscriptEntry[], incoming: readonly TranscriptEntry[]): TranscriptEntry[] {
	if (!incoming.length) return prev
	const byId = new Map(prev.map(entry => [entry.id, entry]))
	let changed = false
	for (const entry of incoming) {
		const existing = byId.get(entry.id)
		if (!existing || (entry.question && entry.rowid < existing.rowid)) {
			byId.set(entry.id, entry)
			changed = true
		}
	}
	return changed ? [...byId.values()].sort((a, b) => a.rowid - b.rowid || partIndex(a.id) - partIndex(b.id)) : prev
}

/** Refresh a preloaded copy with durable entries that arrived while its menu stayed open. */
export function currentCopyEntries(
	full: readonly TranscriptEntry[],
	current: readonly TranscriptEntry[]
): TranscriptEntry[] {
	const latest = new Map(current.filter(entry => !entry.queued).map(entry => [entry.id, entry]))
	const saved = full
		.filter(entry => !entry.queued)
		.map(entry => {
			const update = latest.get(entry.id)
			if (!update) return entry
			// The preload can be ahead of the next poll, or carry a result outside the
			// visible window. An incomplete context call must not erase that output.
			return update.tool && !('output' in update) && 'output' in entry ? entry : update
		})
	const ids = new Set(saved.map(entry => entry.id))
	return mergeMessagePages(
		saved,
		[...latest.values()].filter(entry => !ids.has(entry.id))
	)
}

/** Reuse unchanged folded entries so older-page loading does not redraw every code block. */
export function foldMessagePages(prev: readonly TranscriptEntry[], raw: readonly TranscriptEntry[]): TranscriptEntry[] {
	const existing = new Map(prev.map(entry => [entry.id, entry]))
	return mergeEntries([], raw).map(entry => {
		const old = existing.get(entry.id)
		const keys = Object.keys(entry) as (keyof TranscriptEntry)[]
		return old && keys.length === Object.keys(old).length && keys.every(key => entry[key] === old[key]) ? old : entry
	})
}

/**
 * Fold each tool result onto the call it answers.
 *
 * A call and its output are two `session_messages` rows, and the transcript is polled
 * with a rowid cursor, so anything slower than the 1s tick lands in a later fetch than
 * the row it belongs to. The relay therefore ships the result as its own entry naming
 * the call (`toolUseId`, src/transcript/parser.ts) and the pairing happens here, against every
 * row already on screen rather than within one batch.
 *
 * Identity is the property this has to hold: every row re-renders when its entry object
 * changes and this runs on the 1s poll, so an untouched entry comes back as the same
 * object and only a call row that just gained output is rebuilt.
 *
 * An unpaired result belongs to a call this transcript never carried. A failure still
 * shows on its own — dropping it would read as a step that worked — while a success is
 * output with nothing to attach it to, so it goes.
 */
export function mergeEntries(
	prev: readonly TranscriptEntry[],
	incoming: readonly TranscriptEntry[]
): TranscriptEntry[] {
	const questionIds = new Set(prev.filter(e => e.question).map(e => `${e.question?.provider}:${e.question?.id}`))
	incoming = incoming.filter(e => {
		if (!e.question) return true
		const key = `${e.question.provider}:${e.question.id}`
		if (questionIds.has(key)) return false
		questionIds.add(key)
		return true
	})
	const results = incoming.filter(e => isToolResult(e) && e.toolUseId)
	if (!results.length) return [...prev, ...incoming]

	const next = [...prev, ...incoming.filter(e => !isToolResult(e))]
	const calls = new Map<string, number>()
	next.forEach((e, i) => {
		if (e.tool && e.toolUseId && !calls.has(e.toolUseId)) calls.set(e.toolUseId, i)
	})
	for (const result of results) {
		const at = result.toolUseId ? calls.get(result.toolUseId) : undefined
		if (at === undefined) {
			if (result.error) next.push(result)
			continue
		}
		const call = next[at]
		next[at] = {
			...call,
			output: result.output,
			diff: result.diff,
			images: result.images,
			error: result.error ?? call.error
		}
	}
	return next
}

/**
 * Append the current outbox snapshot to durable transcript rows.
 *
 * The snapshot is replaced on every poll rather than merged: an outbox row vanishes
 * when it is dispatched or cancelled. A dispatch can straddle the two SQLite reads,
 * so prefer the durable row when both snapshots briefly carry the same message id.
 */
export function withQueuedEntries(
	entries: readonly TranscriptEntry[],
	queued: readonly TranscriptEntry[]
): TranscriptEntry[] {
	if (!queued.length) return [...entries]
	const durableIds = new Set(entries.map(entry => entry.id))
	return [...entries, ...queued.filter(entry => !durableIds.has(entry.id))]
}
