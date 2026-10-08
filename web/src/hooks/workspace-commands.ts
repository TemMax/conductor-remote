import { ChartPie, FileDiff, MessageSquarePlus } from 'lucide-react'
import { useMemo, useRef } from 'react'
import { type Command, useRegisterCommands } from '../lib/commands.ts'
import { SETTABLE_STATUSES, workspaceStatusLabel } from '../lib/format.ts'

/** Register the workspace palette while keeping live handlers behind one stable ref. */
export function useWorkspaceCommands({
	statusNow,
	diffOpen,
	canCreateChat,
	canSetStatus,
	hasChat,
	setStatus
}: {
	statusNow: string | null
	diffOpen: boolean
	canCreateChat: boolean
	canSetStatus: boolean
	hasChat: boolean
	setStatus: (status: string) => Promise<void>
}) {
	const latest = useRef<{
		toggleDiff: () => void
		createChat: () => Promise<void>
		openContext: () => void
	} | null>(null)
	const workspaceCommands = useMemo<Command[]>(() => {
		if (!statusNow) return []
		return [
			{
				id: 'workspace.diff',
				label: diffOpen ? 'Hide changes' : 'Show changes',
				group: 'Workspace',
				icon: FileDiff,
				keywords: ['diff', 'files', 'review', 'patch'],
				run: () => latest.current?.toggleDiff()
			},
			{
				id: 'workspace.newChat',
				label: 'New chat',
				group: 'Workspace',
				icon: MessageSquarePlus,
				keywords: ['tab', 'session', 'same files'],
				enabled: canCreateChat,
				run: () => latest.current?.createChat()
			},
			{
				id: 'workspace.context',
				label: 'Context breakdown',
				group: 'Workspace',
				icon: ChartPie,
				keywords: ['tokens', 'window', 'usage'],
				enabled: hasChat,
				run: () => latest.current?.openContext()
			},
			...SETTABLE_STATUSES.map(
				(status): Command => ({
					id: `workspace.status.${status}`,
					label: `Status: ${workspaceStatusLabel(status)}`,
					group: 'Workspace',
					keywords: ['mark', 'move', 'sidebar', 'group'],
					checked: status === statusNow,
					enabled: canSetStatus,
					run: () => {
						if (status !== statusNow) void setStatus(status)
					}
				})
			)
		]
	}, [statusNow, diffOpen, canCreateChat, canSetStatus, hasChat, setStatus])
	useRegisterCommands('workspace', workspaceCommands)
	return latest
}
