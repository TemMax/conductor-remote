import type { TranscriptNode } from '../../lib/transcript/tree.ts'
import type { TranscriptEntry } from '../../lib/types.ts'

export type Row =
	| { kind: 'entry'; key: string; node: TranscriptNode }
	| { kind: 'steps'; key: string; nodes: TranscriptNode[] }

export const rowKey = (e: TranscriptEntry) => `${e.rowid}-${e.id}`

/**
 * Fold each run of the agent's own work (thinking + tool calls) between two
 * spoken messages into one collapsible group. Singleton groups render inline,
 * but keep the same component structure when backfilling extends the run.
 */
export function groupSteps(nodes: TranscriptNode[], previous: readonly Row[] = []): Row[] {
	const rows: Row[] = []
	let run: TranscriptNode[] = []
	const flush = () => {
		if (run.length) rows.push({ kind: 'steps', key: `steps-${rowKey(run[0].e)}`, nodes: run })
		run = []
	}
	for (const node of nodes) {
		// An Agent call is already a named doorway to its child subtab. Folding that into
		// an opaque "N steps" disclosure would hide the feature.
		if (!node.e.question && !node.e.subagentLabel && (node.e.role === 'tool' || node.e.role === 'thinking')) {
			run.push(node)
			continue
		}
		flush()
		rows.push({ kind: 'entry', key: rowKey(node.e), node })
	}
	flush()
	// Backfilling may extend a group at its beginning. Reuse its existing key so
	// React preserves the disclosure's open state and the reader's scroll anchor.
	const owners = new Map<string, string>()
	for (const row of previous) {
		if (row.kind === 'steps') for (const node of row.nodes) owners.set(node.e.id, row.key)
	}
	const used = new Set<string>()
	for (const row of rows) {
		if (row.kind !== 'steps') continue
		const key = row.nodes.map(node => owners.get(node.e.id)).find(key => key && !used.has(key))
		if (key) row.key = key
		used.add(row.key)
	}
	return rows
}
