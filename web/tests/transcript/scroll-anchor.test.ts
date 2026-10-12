import { expect, it } from 'vitest'
import { captureTranscriptAnchor, restoreTranscriptAnchor } from '../../src/lib/transcript/scroll-anchor.ts'

it('keeps a visible step fixed when backfilling extends its open group internally', () => {
	let childTop = 20
	const child = {
		isConnected: true,
		getBoundingClientRect: () => ({ top: childTop, bottom: childTop + 80 })
	}
	const group = {
		getBoundingClientRect: () => ({ top: -80, bottom: 800 }),
		querySelectorAll: () => [child]
	}
	const scroller = {
		scrollTop: 100,
		scrollHeight: 1000,
		getBoundingClientRect: () => ({ top: 0, bottom: 600 }),
		querySelectorAll: () => [group],
		contains: (element: unknown) => element === child
	} as unknown as HTMLElement
	const anchor = captureTranscriptAnchor(scroller)
	childTop += 400
	Object.assign(scroller, { scrollHeight: 1400 })
	restoreTranscriptAnchor(scroller, anchor)
	expect(scroller.scrollTop).toBe(500)
})
