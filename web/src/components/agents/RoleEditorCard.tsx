import { AlertTriangle, Trash2, Zap } from 'lucide-react'
import {
	agentTypeCanExposeEffort,
	agentTypeCanExposeFastMode,
	modelAgentType,
	modelPickerLabel,
	savedModelCatalog
} from '../../contract/shared.ts'
import { EFFORT_LABELS } from '../../lib/agent.ts'
import { cn } from '../../lib/cn.ts'
import type { AgentEffort, AutoModelTuple, CachedModelGroup } from '../../lib/types.ts'
import { EffortBars, ProviderMark } from './AgentIcons.tsx'
import { ModelPicker } from './ModelPicker.tsx'

export const ROLE_EFFORTS: AgentEffort[] = ['none', 'low', 'medium', 'high', 'xhigh', 'max', 'ultracode']

export function nextRoleEffort(effort: AgentEffort | undefined, agentType: string | null): AgentEffort | undefined {
	const efforts = agentType === 'codex' ? ROLE_EFFORTS : ROLE_EFFORTS.filter(value => value !== 'none')
	const choices: Array<AgentEffort | undefined> = [undefined, ...efforts]
	const current = choices.indexOf(effort)
	return choices[(current + 1) % choices.length]
}

export function roleWithEffort(role: AutoModelTuple, effort: AgentEffort | undefined): AutoModelTuple {
	const next = { ...role }
	if (effort === undefined) delete next.effort
	else next.effort = effort
	return next
}

/** Selecting a provider also drops settings that provider cannot render. */
export function roleWithModel(role: AutoModelTuple, model: string): AutoModelTuple {
	const next: AutoModelTuple = { ...role, model }
	const agentType = modelAgentType(model)
	if (!agentTypeCanExposeEffort(agentType)) delete next.effort
	if (!agentTypeCanExposeFastMode(agentType)) delete next.fast
	return next
}

export function roleModelProblem(role: AutoModelTuple, groups: CachedModelGroup[]): string | null {
	if (!savedModelCatalog(groups).includes(modelPickerLabel(role.model))) {
		return 'Choose an exact model from Conductor’s saved picker catalog.'
	}
	const agentType = modelAgentType(role.model)
	if (!agentType) return 'This model label does not identify a supported provider.'
	if (role.effort !== undefined && !agentTypeCanExposeEffort(agentType)) {
		return 'Conductor does not expose a reasoning control for this provider. Select its model again to clear Effort.'
	}
	if (role.effort === 'none' && agentType !== 'codex') return 'None effort is available only for Codex.'
	if (role.fast !== undefined && !agentTypeCanExposeFastMode(agentType)) {
		return 'Conductor does not expose a Fast control for this provider. Select its model again to clear Fast.'
	}
	return null
}

/** Pure role row, exported so invalid/stale-model behavior can be checked without a browser. */
export function RoleEditorCard({
	name,
	role,
	models,
	agentType = null,
	invalid,
	onChange,
	onRemove,
	canRemove
}: {
	name: string
	role: AutoModelTuple
	models: string[]
	agentType?: string | null
	invalid?: string | null
	onChange: (role: AutoModelTuple) => void
	onRemove: () => void
	canRemove: boolean
}) {
	const effectiveAgentType = agentType ?? modelAgentType(role.model) ?? null
	const effortAvailable = agentTypeCanExposeEffort(effectiveAgentType)
	const fastAvailable = agentTypeCanExposeFastMode(effectiveAgentType)

	return (
		<section className={cn('rounded-2xl border bg-surface p-3', invalid ? 'border-del/50' : 'border-border')}>
			<div className="mb-2 flex items-center gap-2">
				<span className="max-w-20 shrink-0 truncate rounded bg-accent/10 px-1 py-0.5 font-mono text-[9px] uppercase tracking-wide text-accent">
					{name}
				</span>
				<span className="min-w-0 flex-1 truncate text-sm font-semibold">{name}</span>
				<button
					type="button"
					disabled={!canRemove}
					onClick={onRemove}
					aria-label={`Remove ${name} role`}
					className="flex size-7 shrink-0 items-center justify-center rounded-lg text-faint active:bg-surface-2 disabled:invisible"
				>
					<Trash2 size={14} />
				</button>
			</div>
			<div className="flex min-w-0 flex-wrap items-center gap-1.5">
				<ModelPicker
					value={role.model}
					models={models}
					placement="below"
					empty="No picker models are cached yet."
					onSelect={model => onChange(roleWithModel(role, model))}
					renderTrigger={({ picking, toggle }) => (
						<button
							type="button"
							onClick={toggle}
							aria-label={`Choose model for ${name}, currently ${role.model}`}
							aria-haspopup="menu"
							aria-expanded={picking}
							className={cn(
								'flex h-8 max-w-56 min-w-0 items-center gap-1.5 rounded-lg border px-2 text-xs active:bg-surface-2',
								invalid ? 'border-del text-del' : 'border-border text-muted'
							)}
						>
							<ProviderMark agentType={agentType} model={role.model} className="size-3.5" />
							<span className="truncate">{role.model}</span>
						</button>
					)}
				/>
				{effortAvailable ? (
					<button
						type="button"
						onClick={() => onChange(roleWithEffort(role, nextRoleEffort(role.effort, effectiveAgentType)))}
						aria-label={`Reasoning effort for ${name}: ${role.effort ? EFFORT_LABELS[role.effort] : 'default'}`}
						className="flex h-8 items-center gap-1.5 rounded-lg border border-border px-2 text-xs text-muted active:bg-surface-2"
					>
						<EffortBars effort={role.effort ?? ''} />
						{role.effort ? EFFORT_LABELS[role.effort] : 'Effort'}
					</button>
				) : null}
				{fastAvailable ? (
					<button
						type="button"
						onClick={() => onChange({ ...role, fast: !role.fast })}
						aria-label={`Fast mode for ${name} ${role.fast ? 'on' : 'off'}`}
						aria-pressed={role.fast === true}
						className={cn(
							'flex h-8 items-center gap-1 rounded-lg border border-border px-2 text-xs text-muted active:bg-surface-2',
							role.fast && 'bg-surface-2 text-text'
						)}
					>
						<Zap size={13} /> Fast
					</button>
				) : null}
			</div>
			{invalid ? (
				<p className="mt-2 flex items-start gap-1.5 text-xs text-del">
					<AlertTriangle size={13} className="mt-0.5 shrink-0" />
					<span>{invalid}</span>
				</p>
			) : null}
		</section>
	)
}
