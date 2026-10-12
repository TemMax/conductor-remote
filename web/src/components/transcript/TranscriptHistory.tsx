import { memo, useCallback, useLayoutEffect, useMemo, useState } from 'react'
import { type TranscriptState, useTranscript } from '../../hooks/transcript.ts'
import { useTranscriptRows } from '../../hooks/transcript-rows.ts'
import { client } from '../../lib/api.ts'
import { assistantTurnEnds, latestAssistantForActions, turnOrigin } from '../../lib/transcript/actions.ts'
import { mergeEntries } from '../../lib/transcript/merge.ts'
import { transcriptSubagents, transcriptTree } from '../../lib/transcript/tree.ts'
import { Spinner } from '../ui.tsx'
import { ChatActions } from './ChatActions.tsx'
import { NodeEntry, StepGroup, SubagentResult, subagentFinal } from './entries.tsx'
import { rowKey } from './grouping.ts'
import type { SplitFormat } from './types.ts'

/** Earlier contexts share the live transcript's scroller and renderer, never an attachment viewer. */
export const TranscriptHistory = memo(function TranscriptHistory({
	sessionId,
	onLayout,
	onPagination,
	onFork
}: {
	sessionId: string
	onLayout?: (loading: boolean) => void
	onPagination?: (state: TranscriptState) => void
	onFork?: (format: SplitFormat) => Promise<void>
}) {
	// The completed context is read once. Only the newest chat keeps the 1s poll.
	const transcript = useTranscript(sessionId, false)
	const entries = useMemo(() => transcript.entries.filter(entry => !entry.queued), [transcript.entries])
	const nodes = useMemo(() => transcriptTree(entries), [entries])
	const [selectedAgent, selectAgent] = useState<string | null>(null)
	const subagent = useMemo(
		() => transcriptSubagents(nodes).find(agent => agent.id === selectedAgent),
		[nodes, selectedAgent]
	)
	const rows = useTranscriptRows(subagent?.node.children ?? nodes)
	const rootEntries = useMemo(() => nodes.map(node => node.e), [nodes])
	const actions = useMemo(() => new Set(assistantTurnEnds(rootEntries).map(rowKey)), [rootEntries])
	const latest = useMemo(() => latestAssistantForActions(rootEntries), [rootEntries])
	const final = subagent ? subagentFinal(subagent.node) : null

	// Fetching an older segment increases scrollHeight, not the scroller's height.
	// Tell its owner after layout so a reader at the bottom stays with the composer.
	// biome-ignore lint/correctness/useExhaustiveDependencies: content/selection changes drive the layout notification
	useLayoutEffect(
		() => onLayout?.(transcript.loading || transcript.loadingOlder),
		[entries, transcript.loading, transcript.loadingOlder, selectedAgent, onLayout]
	)
	useLayoutEffect(() => onPagination?.(transcript), [transcript, onPagination])
	const loadEntries = useCallback(
		async () => mergeEntries([], (await client.messages(sessionId, 0)).entries),
		[sessionId]
	)

	return (
		<section data-chat-history={sessionId} aria-label="Previous conversation context">
			{transcript.loadingOlder ? <Spinner label="Loading earlier messages…" /> : null}
			{transcript.olderError ? (
				<button type="button" className="py-2 text-xs text-del" onClick={() => void transcript.loadOlder()}>
					Could not load earlier messages. Retry
				</button>
			) : null}
			{transcript.loading ? (
				<Spinner label="Loading earlier conversation…" />
			) : transcript.error ? (
				<p className="py-3 text-xs text-del">Could not load earlier conversation: {transcript.error}</p>
			) : (
				<div className="flex min-w-0 flex-col gap-2.5">
					{subagent ? (
						<button type="button" className="self-start py-2 text-xs text-accent" onClick={() => selectAgent(null)}>
							Back to conversation
						</button>
					) : null}
					{rows.map(row =>
						row.kind === 'steps' ? (
							<div key={row.key} data-transcript-row={row.key}>
								<StepGroup nodes={row.nodes} />
							</div>
						) : (
							<div key={row.key} data-transcript-row={row.key}>
								<NodeEntry node={row.node} onSelectSubagent={selectAgent} />
								{!subagent && actions.has(rowKey(row.node.e)) ? (
									<ChatActions
										text={row.node.e.text}
										entries={entries}
										at={row.node.e.ts}
										startedAt={turnOrigin(rootEntries, row.node.e)}
										rowid={row.node.e.rowid}
										through={row.node.e === latest ? undefined : row.node.e.rowid}
										onFork={onFork ? format => onFork({ ...format, sourceSessionId: sessionId }) : undefined}
										loadEntries={transcript.hasOlder ? loadEntries : undefined}
									/>
								) : null}
							</div>
						)
					)}
					{final ? <SubagentResult text={final} failed={!!subagent?.node.e.error} /> : null}
				</div>
			)}
			<div className="relative my-6">
				<hr className="border-border-soft" />
				<span className="absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 whitespace-nowrap bg-bg px-3 text-[11px] text-faint">
					Context reset
				</span>
			</div>
		</section>
	)
})
