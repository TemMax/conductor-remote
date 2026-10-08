import type { ChatHistoryLink, Session } from '../types.ts'

/** Oldest first, excluding the live chat. Tolerate a stale/corrupt link without looping. */
export function previousChats(sessionId: string | null, links: Record<string, ChatHistoryLink>): string[] {
	const result: string[] = []
	const seen = new Set(sessionId ? [sessionId] : [])
	let id = sessionId ? links[sessionId]?.previousSessionId : undefined
	while (id && !seen.has(id)) {
		seen.add(id)
		result.unshift(id)
		id = links[id]?.previousSessionId
	}
	return result
}

function availableChatResolver(sessions: Session[], links: Record<string, ChatHistoryLink>) {
	const available = new Set(sessions.map(session => session.id))
	const next = new Map(Object.entries(links).map(([id, link]) => [link.previousSessionId, id]))
	return (sessionId: string | null): string | null => {
		const seen = new Set<string>()
		let id = sessionId
		let latest: string | null = null
		while (id && !seen.has(id)) {
			seen.add(id)
			if (available.has(id)) latest = id
			id = next.get(id) ?? null
		}
		// A corrupt cycle must not hide every available context in the conversation.
		if (id && sessionId && available.has(sessionId)) return sessionId
		// A closed continuation can still be selected by a URL or the desktop.
		return latest ?? previousChats(sessionId, links).findLast(previous => available.has(previous)) ?? null
	}
}

/** Links and notifications open the newest available context in the same conversation. */
export function latestChat(
	sessionId: string | null,
	links: Record<string, ChatHistoryLink>,
	sessions: Session[]
): string | null {
	return availableChatResolver(sessions, links)(sessionId)
}

/** Only the newest available context gets a tab; sends still address its real session id. */
export function conversationTabs(sessions: Session[], links: Record<string, ChatHistoryLink>): Session[] {
	const resolve = availableChatResolver(sessions, links)
	return sessions
		.filter(session => resolve(session.id) === session.id)
		.map(session => {
			const link = links[session.id]
			return link ? { ...session, title: link.title || session.title, created_at: link.createdAt } : session
		})
		.sort((a, b) => a.created_at.localeCompare(b.created_at))
}
