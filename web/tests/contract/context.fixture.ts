import type { ContextBreakdownResponse } from '../../src/contract/wire.ts'

/**
 * What `GET /api/sessions/ctx-chat/context` returns for the relay's seeded test database: a chat that
 * Conductor reports at 9,000 tokens (7.5% of the window), compacted once. The counted window is what
 * follows the compaction boundary up to the last completed turn, so the unanswered prompt at the end is
 * left out of the categories but still part of what a fork copies.
 *
 * Of the 9,000 tokens, 57 are prose (the summary, the prompt and two replies, 228 bytes), 18 thinking and 60
 * tool traffic (a call and its result); the rest, 8,865, is initial context. The three fork sizes are the
 * estimates for the whole transcript rendered without thinking and tools, with thinking, and with both.
 * The relay's test produces the same value from the seeded rows and compares it with
 * `fixtures/context-breakdown.json`, which the test beside this file holds equal to this constant.
 */
export const contextBreakdown: ContextBreakdownResponse = {
	totalTokens: 9000,
	usedPercent: 7.5,
	compacted: true,
	categories: { initial: 8865, chat: 57, thinking: 18, tools: 60 },
	forkTokens: { concise: 125, reasoning: 138, full: 177 }
}
