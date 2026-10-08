import { useEffect, useState } from 'react'
import { useModelCatalog, useModels } from '../../hooks/agents.ts'
import { nextEffort, supportsEffortControl, supportsFastMode, supportsPlanMode } from '../../lib/agent.ts'
import { modelLabel } from '../../lib/format.ts'
import type { AgentPatch, Session } from '../../lib/types.ts'
import { useApp } from '../../store.ts'
import { AgentControls } from './AgentControls.tsx'

/**
 * Conductor's own composer controls, mirrored for the phone — and rendered
 * *inside* the composer card (web/src/components/session/Composer.tsx) so the whole thing has one left edge
 * and one border, like the desktop app.
 *
 * Values are read from the DB (durable, like every other read). Changes are
 * **staged, not sent**: pushing one costs a slow, focus-stealing AppleScript trip
 * and only decides what the *next* prompt runs on, so a tap is instant and local,
 * and the send applies it (hooks/send.ts ▸ `useSendPrompt`) before the prompt goes.
 * A staged control uses the accent colour, and flipping a value back to what
 * Conductor already has drops the staged one rather than queuing a no-op trip.
 */
/** Keep the relay cache useful without leaving a Conductor menu stale all day. */
const MODEL_CATALOG_STALE_MS = 10 * 60 * 1000

/** Nothing staged — a stable identity so the selector can't loop. */
const NOTHING: AgentPatch = {}

/** A staged value only exists while it differs from Conductor's; flipping back clears it. */
function change<T>(next: T, current: unknown): T | undefined {
	return next === current ? undefined : next
}

export function AgentBar({
	session,
	workspaceId,
	auto = false,
	onAutoChange
}: {
	session: Session
	workspaceId: string
	auto?: boolean
	onAutoChange?: (active: boolean) => void
}) {
	const [picking, setPicking] = useState(false)
	const staged = useApp(s => s.agentDrafts[session.id]) ?? NOTHING
	const stageAgent = useApp(s => s.stageAgent)
	// A send in flight is what pushes the staged settings. The controls stay live
	// through it — anything changed mid-send simply stages for the next one, which
	// the store's key-wise `clearAgentDraft` is what makes safe.
	const sending = useApp(s => s.pending.some(p => p.sessionId === session.id && p.status === 'sending'))
	const modelCatalog = useModelCatalog()
	const cachedGroup = modelCatalog.data?.groups.find(group => group.agentType === (session.agent_type ?? 'unknown'))
	// Caches written before the relay recorded a default have labels only; refresh those once.
	const cacheFresh = !!cachedGroup?.defaultModel && Date.now() - cachedGroup.updatedAt < MODEL_CATALOG_STALE_MS
	const liveModels = useModels(session, workspaceId, picking && !cacheFresh)
	const models = liveModels.data?.models ?? cachedGroup?.models ?? []

	const stage = (patch: AgentPatch) => stageAgent(session.id, patch)

	const dbEffort = session.claude_effort_level ?? undefined
	const dbPlan = session.permission_mode === 'plan'
	const dbFast = Boolean(session.fast_mode)
	const effort = staged.effort ?? dbEffort
	const planOn = staged.plan ?? dbPlan
	const fastOn = staged.fast ?? dbFast
	const anyStaged = Object.keys(staged).length > 0
	// Named off the picker's own labels when they're loaded: the id says `opus-5-1m`
	// where Conductor's menu says "Opus 5", and a pill that disagrees with the menu
	// also leaves the open picker with no row checked.
	const displayedModel = staged.model ?? (modelLabel(session.model, models) || 'Model')
	const providerModel = staged.model ?? session.model
	const planAvailable = supportsPlanMode(session.agent_type, providerModel)
	const effortAvailable = supportsEffortControl(session.agent_type, providerModel)
	const fastAvailable = supportsFastMode(session.agent_type, providerModel)

	useEffect(() => {
		if (['selecting', 'waiting', 'failed'].includes(session.auto_model?.status ?? '')) setPicking(false)
	}, [session.auto_model?.status])

	// A Plan choice can survive in synced/local drafts after switching away from
	// Claude. Drop it as soon as the effective model no longer has Conductor's
	// control, or the invisible patch would make the next send fail in AppleScript.
	useEffect(() => {
		if (!planAvailable && staged.plan !== undefined) stageAgent(session.id, { plan: undefined })
	}, [planAvailable, session.id, staged.plan, stageAgent])

	// Provider switches can leave an invisible staged setting behind. Cursor and
	// OpenCode have no matching controls, so never carry those settings into send.
	useEffect(() => {
		if (!effortAvailable && staged.effort !== undefined) stageAgent(session.id, { effort: undefined })
		if (!fastAvailable && staged.fast !== undefined) stageAgent(session.id, { fast: undefined })
	}, [effortAvailable, fastAvailable, session.id, staged.effort, staged.fast, stageAgent])

	return (
		<AgentControls
			auto={auto || ['selecting', 'waiting'].includes(session.auto_model?.status ?? '')}
			onAutoChange={onAutoChange}
			disabled={['selecting', 'waiting', 'failed'].includes(session.auto_model?.status ?? '')}
			model={displayedModel}
			providerModel={providerModel}
			agentType={session.agent_type}
			models={models}
			modelPickerOpen={picking}
			onModelPickerOpenChange={setPicking}
			modelsFetching={liveModels.isFetching || modelCatalog.isFetching}
			modelsError={liveModels.isError}
			fast={fastOn}
			effort={effort}
			plan={planOn}
			planAvailable={planAvailable}
			modelStaged={staged.model !== undefined}
			fastStaged={staged.fast !== undefined}
			effortStaged={staged.effort !== undefined}
			planStaged={staged.plan !== undefined}
			onModelChange={model => stage({ model: auto ? model : change(model, staged.model), auto: false })}
			onFastChange={() => stage({ fast: change(!fastOn, dbFast) })}
			onEffortChange={() => stage({ effort: change(nextEffort(effort), dbEffort) })}
			onPlanChange={() => stage({ plan: change(!planOn, dbPlan) })}
			status={
				auto
					? 'Chooses a model from your first message'
					: session.auto_model?.decision
						? `Auto chose ${session.auto_model.decision.model} · ${session.auto_model.decision.reason}`
						: anyStaged
							? sending
								? 'Applying…'
								: 'Applies when you send'
							: undefined
			}
		/>
	)
}
