import { useMemo, useRef } from 'react'
import { groupSteps, type Row } from '../components/transcript/grouping.ts'
import type { TranscriptNode } from '../lib/transcript/tree.ts'

/** Stable disclosure keys across forward polling and backward pagination. */
export function useTranscriptRows(nodes: TranscriptNode[]) {
	const previous = useRef<Row[]>([])
	return useMemo(() => {
		const rows = groupSteps(nodes, previous.current)
		previous.current = rows
		return rows
	}, [nodes])
}
