import { useQueryClient } from '@tanstack/react-query'
import { useCallback } from 'react'
import { useNavigate } from 'react-router'
import { client } from '../lib/api.ts'
import { useApp } from '../store.ts'

/**
 * Send a prompt with an optimistic in-chat bubble. Adds a `sending` pending
 * immediately — also the workspace/chat status ring's queue signal — then relies
 * on the relay's delivery read-back: on `ok` the real
 * user row lands via the transcript poll and reconciles the bubble away (a
 * fallback purge covers the rare no-match); on failure the bubble flips to an
 * inline error with Retry. Reused by the Composer and the Transcript's Retry
 * button — pass the pending's `id` to retry in place. There is no green "Sent"
 * toast: a delivered prompt simply appears in the chat, a failed one shows an error.
 *
 * This is also where a staged agent change lands (store ▸ `agentDrafts`): the
 * patch rides in the send request itself and the relay applies it *before* the
 * prompt, which only goes if that succeeded — running a prompt on the model the
 * user just moved away from is the same class of mistake as landing it in the
 * wrong workspace, so it fails loud with the settings still staged and the text
 * still in the bubble, one Retry from going again. One request, not two, so a
 * locked Mac parks the patch and the prompt together.
 *
 * `parked` is the relay saying the Mac is locked and it has taken the prompt
 * (src/delivery/parked.ts): treated like a send that landed — composer clears, drafts
 * clear (the parked entry carries them) — except nothing is `working`; the
 * queued bubble from `/api/state` shows what is still owed.
 *
 * A prompt that moved to a new chat takes the phone there.
 */
export function useSendPrompt() {
	const queryClient = useQueryClient()
	const navigate = useNavigate()
	const addPending = useApp(s => s.addPending)
	const failPending = useApp(s => s.failPending)
	const removePending = useApp(s => s.removePending)
	const markWorking = useApp(s => s.markWorking)
	const clearAgentDraft = useApp(s => s.clearAgentDraft)

	return useCallback(
		async (opts: {
			auto?: boolean
			id?: string
			sessionId: string
			workspaceId: string
			text: string
			queue?: boolean
		}): Promise<boolean> => {
			const text = opts.text.trim()
			if (!text) return false
			const { sessionId, workspaceId } = opts
			const id = opts.id ?? crypto.randomUUID()
			const auto = opts.auto ?? useApp.getState().agentDrafts[sessionId]?.auto ?? false
			addPending({ id, sessionId, workspaceId, text, queue: opts.queue, auto })
			try {
				// Read at send time, not through the closure: the user may have changed the
				// model between mounting the composer and tapping send.
				const staged = useApp.getState().agentDrafts[sessionId]
				const manual = staged && Object.fromEntries(Object.entries(staged).filter(([key]) => key !== 'auto'))
				const agent = !auto && manual && Object.keys(manual).length ? manual : undefined
				// `id` goes to the relay as well as into the bubble: it is the send's identity,
				// so a Retry of this same bubble is answered rather than sent again. Which is
				// the duplicate the chats here hold — the prompt landed, the answer didn't.
				const r = await client.sendPrompt(sessionId, text, workspaceId, agent, id, opts.queue, auto)
				if (r.ok || r.parked) {
					const landedIn = r.ok && r.sessionId && r.sessionId !== sessionId ? r.sessionId : undefined
					const appliedDraft = auto ? staged : { ...agent, auto: staged?.auto }
					if (appliedDraft) {
						// Ordinary choices were applied or parked with the prompt.
						clearAgentDraft(sessionId, appliedDraft)
						queryClient.invalidateQueries({ queryKey: ['sessions', workspaceId] })
					}
					if (r.ok) markWorking(landedIn ?? sessionId)
					// A send by hand supersedes any first prompt the relay was still holding, and
					// the relay drops that entry — so re-read the list the queued bubble comes from.
					// A parked send is the reverse: the re-read is what *shows* its queued bubble.
					queryClient.invalidateQueries({ queryKey: ['state'] })
					// The confirmed row (or the parked entry) surfaces on the next poll and hides
					// this bubble; purge it after a beat so a text-match miss can't leave a duplicate.
					if (landedIn) {
						removePending(id)
						queryClient.invalidateQueries({ queryKey: ['sessions', workspaceId] })
						navigate(`/w/${workspaceId}?session=${encodeURIComponent(landedIn)}`, { replace: true })
					} else {
						setTimeout(() => removePending(id), 4000)
					}
					return true
				}
				failPending(id, r.error || 'Send failed')
			} catch (err) {
				failPending(id, err instanceof Error ? err.message : String(err))
			}
			return false
		},
		[addPending, failPending, removePending, markWorking, clearAgentDraft, queryClient, navigate]
	)
}
