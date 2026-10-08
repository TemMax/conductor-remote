import type { StateResponse } from '../../contract/wire.ts'
import { Empty } from '../ui.tsx'

/** How long a launch request is shown as in progress while Conductor starts. */
export const LAUNCH_GRACE_MS = 20_000

/** A missing `conductor` field means running: an older relay never sent it. */
export function isConductorOffline(state: StateResponse | undefined): boolean {
	return state?.conductor?.running === false
}

/** Shown in place of the workspace list while the relay reports Conductor is not running. */
export function ConductorOffline({
	onLaunch,
	pending,
	error
}: {
	onLaunch: () => void
	pending: boolean
	error: string | null
}) {
	return (
		<Empty>
			<div className="font-medium text-text">Conductor is not running</div>
			<div className="mt-1">Open it on your Mac to see your workspaces.</div>
			<button
				type="button"
				onClick={onLaunch}
				disabled={pending}
				className="mt-4 w-full rounded-2xl bg-accent px-4 py-3 text-[15px] font-semibold text-bg transition active:scale-[0.985] disabled:opacity-40"
			>
				{pending ? 'Launching…' : 'Launch Conductor'}
			</button>
			{error ? <div className="mt-3 break-words text-del">{error}</div> : null}
		</Empty>
	)
}
