import { Check, ChevronDown, RefreshCw, Settings2, Sparkles } from 'lucide-react'
import { type ReactNode, useState } from 'react'
import { displayedModelPickerLabel, groupModelPickerLabels } from '../../contract/shared.ts'
import { cn } from '../../lib/cn.ts'

type ModelPickerTrigger = {
	picking: boolean
	toggle: () => void
}

/**
 * Shared picker chrome for an existing chat and for the new-workspace form.
 * The caller owns model loading because a chat can refresh the live Conductor
 * menu, while a new workspace can only read the relay's stored labels.
 */
export function ModelPicker({
	value,
	models,
	onSelect,
	open,
	onOpenChange,
	isFetching = false,
	isError = false,
	empty = 'No models are cached yet. Open a model picker in a chat first.',
	placement = 'above',
	className,
	beforeOptions,
	autoSelected,
	onSelectAuto,
	onAutoSettings,
	renderTrigger
}: {
	value?: string
	models: string[]
	onSelect: (model: string) => void
	open?: boolean
	onOpenChange?: (open: boolean) => void
	isFetching?: boolean
	isError?: boolean
	empty?: string
	/** Composer controls open upward; settings rows have room below them. */
	placement?: 'above' | 'below'
	className?: string
	/** Controls shown above the model choices, such as an existing chat's agent settings. */
	beforeOptions?: ReactNode
	autoSelected?: boolean
	onSelectAuto?: () => void
	onAutoSettings?: () => void
	renderTrigger?: (trigger: ModelPickerTrigger) => ReactNode
}) {
	const [internalOpen, setInternalOpen] = useState(false)
	const picking = open ?? internalOpen
	const setPicking = (next: boolean | ((current: boolean) => boolean)) => {
		const resolved = typeof next === 'function' ? next(picking) : next
		if (open === undefined) setInternalOpen(resolved)
		onOpenChange?.(resolved)
	}
	const groups = groupModelPickerLabels(models)

	return (
		<div className="relative">
			{renderTrigger ? (
				renderTrigger({ picking, toggle: () => setPicking(open => !open) })
			) : (
				<button
					type="button"
					onClick={() => setPicking(open => !open)}
					aria-haspopup="menu"
					aria-expanded={picking}
					className={cn('ctl flex max-w-40 items-center gap-1', value && 'ctl-staged', className)}
				>
					<span className="truncate">{value ? displayedModelPickerLabel(value) : 'Model'}</span>
					<ChevronDown size={13} className="shrink-0" />
				</button>
			)}
			{picking ? (
				<>
					<button
						type="button"
						aria-label="Close model picker"
						onClick={() => setPicking(false)}
						className="fixed inset-0 z-30 cursor-default"
					/>
					<div
						className={cn(
							'absolute left-0 z-40 max-h-64 w-64 overflow-y-auto rounded-xl border border-border bg-surface-2 py-1 shadow-xl shadow-black/40',
							placement === 'below' ? 'top-full mt-2' : 'bottom-full mb-2'
						)}
					>
						{beforeOptions}
						{onSelectAuto ? (
							<div className="flex items-stretch border-b border-border-soft">
								<button
									type="button"
									className="flex flex-1 items-center gap-2 px-3 py-2 text-left text-sm"
									onClick={() => {
										setPicking(false)
										onSelectAuto()
									}}
								>
									<Sparkles size={14} />
									<span className="flex-1">Auto</span>
									{autoSelected ? <Check size={13} className="text-accent" /> : null}
								</button>
								{onAutoSettings ? (
									<button
										type="button"
										aria-label="Auto model settings"
										className="px-3 text-muted"
										onClick={() => {
											setPicking(false)
											onAutoSettings()
										}}
									>
										<Settings2 size={15} />
									</button>
								) : null}
							</div>
						) : null}
						{isFetching ? <RefreshCw size={10} className="mx-3 my-1.5 animate-spin text-faint" /> : null}
						{groups.length ? (
							groups.map(group => (
								<fieldset key={group.label} className="m-0 min-w-0 border-0 p-0">
									<legend className="px-3 pb-0.5 pt-1.5 text-[11px] font-medium text-faint">{group.label}</legend>
									{group.models.map(model => (
										<div key={model} className="flex items-stretch active:bg-surface">
											<button
												type="button"
												onClick={() => {
													setPicking(false)
													onSelect(model)
												}}
												className="flex min-w-0 flex-1 items-center gap-2 py-2 pl-3 pr-1 text-left text-sm"
											>
												<span className="min-w-0 flex-1 truncate">{displayedModelPickerLabel(model)}</span>
												<Check size={13} className={cn('shrink-0 text-accent', value !== model && 'invisible')} />
											</button>
										</div>
									))}
								</fieldset>
							))
						) : (
							<div className="px-3 py-2 text-sm text-muted">
								{isFetching ? 'Reading Conductor’s model list…' : empty}
							</div>
						)}
						{isError ? (
							<div className="px-3 py-1.5 text-[11px] text-del">
								{models.length ? 'Couldn’t refresh. Showing saved models.' : 'Couldn’t read the model list.'}
							</div>
						) : null}
					</div>
				</>
			) : null}
		</div>
	)
}
