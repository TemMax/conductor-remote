import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { client } from '../lib/api.ts'
import { foldMessagePages, mergeMessagePages, withQueuedEntries } from '../lib/transcript/merge.ts'
import type { MessagesResponse, QuestionRequest, TranscriptEntry } from '../lib/types.ts'
import { useApp } from '../store.ts'
import { useOnline } from './browser.ts'

export interface TranscriptState {
	pendingQuestion: QuestionRequest | null
	entries: TranscriptEntry[]
	loading: boolean
	error: string | null
	hasOlder: boolean
	loadingOlder: boolean
	olderError: string | null
	loadOlder: () => Promise<void>
}

interface PollState extends Omit<TranscriptState, 'loadOlder'> {
	queued: TranscriptEntry[]
}
const emptyState = (): PollState => ({
	entries: [],
	queued: [],
	pendingQuestion: null,
	loading: true,
	error: null,
	hasOlder: false,
	loadingOlder: false,
	olderError: null
})
const sameQueuedEntries = (a: readonly TranscriptEntry[], b: readonly TranscriptEntry[]) =>
	a.length === b.length && a.every((entry, index) => entry.id === b[index].id && entry.text === b[index].text)

/** Newest window first; forward polls and explicit backward reads share one flight. */
export function useTranscript(sessionId: string | null, poll = true): TranscriptState {
	const report = useOnline()
	const [state, setState] = useState<PollState>(emptyState)
	const olderLoader = useRef<() => Promise<void>>(async () => {})
	const loadOlder = useCallback(() => olderLoader.current(), [])
	const readingAs = useApp(s => s.push.deviceId)
	const readingRef = useRef(readingAs)
	readingRef.current = readingAs

	useEffect(() => {
		if (!sessionId) {
			setState({ ...emptyState(), loading: false })
			olderLoader.current = async () => {}
			return
		}
		setState(emptyState())
		let alive = true
		let cursor = 0
		let oldest = 0
		let initialized = false
		let hasOlder = false
		let raw: TranscriptEntry[] = []
		let flight: Promise<void> | null = null
		let olderFlight: Promise<void> | null = null

		const reading = () => (document.visibilityState === 'visible' ? readingRef.current : null)
		const publish = (data: MessagesResponse, mode: 'initial' | 'forward' | 'older') => {
			if (!alive) return
			const previousRaw = raw
			raw = mergeMessagePages(raw, [...(data.contextEntries ?? []), ...data.entries])
			const nextRaw = raw
			if (mode !== 'older') cursor = data.cursor
			if (mode !== 'forward') {
				oldest = data.oldestCursor ?? 0
				hasOlder = data.hasOlder ?? false
			}
			const queued = data.queued ?? []
			const question = data.pendingQuestion ?? null
			const olderAvailable = hasOlder
			setState(prev => {
				const entries = nextRaw === previousRaw ? prev.entries : foldMessagePages(prev.entries, nextRaw)
				const sameQueue = sameQueuedEntries(prev.queued, queued)
				const sameQuestion = JSON.stringify(prev.pendingQuestion) === JSON.stringify(question)
				if (
					entries === prev.entries &&
					sameQueue &&
					sameQuestion &&
					prev.hasOlder === olderAvailable &&
					!prev.loading &&
					!prev.error
				)
					return prev
				return {
					...prev,
					entries,
					queued: sameQueue ? prev.queued : queued,
					pendingQuestion: sameQuestion ? prev.pendingQuestion : question,
					loading: false,
					error: null,
					hasOlder: olderAvailable
				}
			})
			report(true)
		}
		const tick = () => {
			if (flight || olderFlight || !alive) return
			flight = (async () => {
				try {
					let more: boolean
					do {
						const initial = !initialized
						const data = await client.messagePage(sessionId, initial ? {} : { after: cursor }, reading())
						if (!alive) return
						publish(data, initial ? 'initial' : 'forward')
						initialized = true
						more = data.hasMore === true
					} while (more && alive)
				} catch (err) {
					if (!alive) return
					report(false, err)
					setState(prev => ({ ...prev, loading: false, error: err instanceof Error ? err.message : String(err) }))
				} finally {
					flight = null
				}
			})()
		}
		olderLoader.current = () => {
			if (olderFlight) return olderFlight
			if (!alive || !initialized || !hasOlder) return Promise.resolve()
			const previousFlight = flight
			setState(prev => ({ ...prev, loadingOlder: true, olderError: null }))
			olderFlight = (async () => {
				try {
					await previousFlight
					if (!alive) return
					const data = await client.messagePage(sessionId, { before: oldest }, reading())
					publish(data, 'older')
				} catch (err) {
					if (alive) setState(prev => ({ ...prev, olderError: err instanceof Error ? err.message : String(err) }))
				} finally {
					olderFlight = null
					flight = null
					if (alive) setState(prev => ({ ...prev, loadingOlder: false }))
				}
			})()
			flight = olderFlight
			return olderFlight
		}
		tick()
		const timer = poll ? setInterval(tick, 1000) : undefined
		return () => {
			alive = false
			if (timer) clearInterval(timer)
		}
	}, [sessionId, poll, report])

	const entries = useMemo(() => withQueuedEntries(state.entries, state.queued), [state.entries, state.queued])
	return { ...state, entries, loadOlder }
}
