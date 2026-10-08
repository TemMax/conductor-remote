/**
 * Conductor's merge button, on the phone, in the intended flow: an agent opens
 * the PR, then you tap merge to merge *the PR*. This is a GitHub write via `gh`
 * (the same outward reach as src/git/pr.ts, which already reads PR state) — GitHub does
 * the merge server-side, so nothing local is pushed or checked out. The button
 * only exists when there's an open PR (see the PWA), so this never invents one.
 */

export type MergeMethod = 'squash' | 'merge' | 'rebase'

export interface MergeResult {
	ok: boolean
	branch: string
	method?: MergeMethod
	error?: string
}
