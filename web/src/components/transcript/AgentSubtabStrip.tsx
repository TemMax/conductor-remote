import { AlertTriangle, ArrowRight, CheckCircle2, Hourglass, Loader2 } from 'lucide-react'
import { displayedModelPickerLabel } from '../../contract/shared.ts'
import { cn } from '../../lib/cn.ts'
import { ProviderMark } from '../agents/AgentIcons.tsx'

/** One virtual provider-native child in the agent strip. */
export interface AgentSubtab {
	key: string
	label: string
	model: string | null
	agentType: string | null
	status?: string
	state?: 'busy' | 'waiting' | 'failed' | 'done'
	selected?: boolean
	onSelect?: () => void
}

/**
 * The product's second navigation level: compact provider-aware agent tabs.
 *
 * Native subagents are addressed by their spawning tool ids. Keeping the
 * presentation address-agnostic preserves the visual hierarchy without
 * pretending a native child is a promptable Conductor chat.
 */
export function AgentSubtabStrip({ tabs, label }: { tabs: AgentSubtab[]; label: string }) {
	if (!tabs.length) return null
	return (
		<nav
			aria-label={label}
			className="flex shrink-0 items-center gap-1.5 overflow-x-auto border-b border-border-soft bg-bg px-3 py-2"
		>
			{tabs.map((tab, index) => {
				const failed = tab.state === 'failed'
				const contents = (
					<>
						<ProviderMark agentType={tab.agentType} model={tab.model} monochrome={tab.selected} className="size-3.5" />
						<span className="max-w-28 truncate font-medium">{tab.label}</span>
						{tab.model ? (
							<span className={cn('max-w-28 truncate', tab.selected ? 'text-bg/75' : 'text-faint')}>
								{displayedModelPickerLabel(tab.model)}
							</span>
						) : null}
						{tab.status ? (
							<span
								className={cn(
									'flex shrink-0 items-center gap-1',
									tab.selected ? 'text-bg/75' : failed ? 'text-del' : 'text-muted'
								)}
							>
								{tab.state === 'failed' ? (
									<AlertTriangle size={10} />
								) : tab.state === 'waiting' ? (
									<Hourglass size={10} />
								) : tab.state === 'done' ? (
									<CheckCircle2 size={10} />
								) : (
									<Loader2 size={10} className="animate-spin" />
								)}
								{tab.status}
							</span>
						) : null}
					</>
				)
				return (
					<div key={tab.key} className="flex shrink-0 items-center gap-1.5">
						{index ? <ArrowRight size={11} className="shrink-0 text-faint" /> : null}
						{tab.onSelect ? (
							<button
								type="button"
								onClick={tab.onSelect}
								aria-current={tab.selected ? 'page' : undefined}
								className={cn(
									'flex h-7 shrink-0 items-center gap-1.5 rounded-lg border px-2 text-[11px]',
									tab.selected
										? 'border-text bg-text text-bg active:bg-text/90'
										: 'border-border-soft active:bg-surface-2',
									failed && !tab.selected && 'border-del/40'
								)}
							>
								{contents}
							</button>
						) : (
							<div
								className={cn(
									'flex h-7 shrink-0 items-center gap-1.5 rounded-lg border border-border-soft px-2 text-[11px]',
									failed && 'border-del/40'
								)}
							>
								{contents}
							</div>
						)}
					</div>
				)
			})}
		</nav>
	)
}
