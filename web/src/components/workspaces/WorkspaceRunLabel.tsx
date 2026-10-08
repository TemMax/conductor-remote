import { modelLabel } from '../../lib/format.ts'
import type { CachedModelGroup, Workspace } from '../../lib/types.ts'
import { ProviderMark } from '../agents/AgentIcons.tsx'

type WorkspaceRun = Pick<Workspace, 'agent_type' | 'model'>

/**
 * The picker labels to name this workspace's model with. Its own agent's list when
 * that picker has been read, and otherwise everything the relay has ever seen — an
 * id that several of those labels could name resolves to none of them
 * (`format.ts` ▸ `modelLabel`), so the wider list can't produce a wrong name.
 */
function catalogFor(groups: CachedModelGroup[] | undefined, agentType: string | null): string[] {
	if (!groups?.length) return []
	const own = groups.find(group => group.agentType === (agentType ?? 'unknown'))
	return own?.models ?? [...new Set(groups.flatMap(group => group.models))]
}

/** A workspace is named by the model its active chat runs on. */
export function WorkspaceRunLabel({
	workspace,
	modelGroups
}: {
	workspace: WorkspaceRun
	modelGroups: CachedModelGroup[] | undefined
}) {
	const model = modelLabel(workspace.model, catalogFor(modelGroups, workspace.agent_type))
	if (!model) return null
	return (
		<span className="ml-auto flex min-w-0 items-center gap-1 text-[11px]">
			<ProviderMark agentType={workspace.agent_type} model={workspace.model} className="size-3" />
			<span className="truncate">{model}</span>
		</span>
	)
}
