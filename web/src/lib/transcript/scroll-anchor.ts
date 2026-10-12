export interface TranscriptScrollAnchor {
	element: HTMLElement | null
	top: number
	height: number
	offset: number
}

/** Anchor an existing row, so concurrent content below it does not move the viewport. */
export function captureTranscriptAnchor(scroller: HTMLElement): TranscriptScrollAnchor {
	const bounds = scroller.getBoundingClientRect()
	const visible = (row: HTMLElement) => {
		const rect = row.getBoundingClientRect()
		return rect.bottom > bounds.top && rect.top < bounds.bottom
	}
	const row = [...scroller.querySelectorAll<HTMLElement>('[data-transcript-row]')].find(visible)
	// A group may grow internally while its outer top stays fixed. Anchor its
	// visible child, so prepending steps preserves the actual content being read.
	const element = row ? ([...row.querySelectorAll<HTMLElement>('[data-transcript-step]')].find(visible) ?? row) : null
	return {
		element,
		top: scroller.scrollTop,
		height: scroller.scrollHeight,
		offset: element ? element.getBoundingClientRect().top - bounds.top : 0
	}
}

export function restoreTranscriptAnchor(scroller: HTMLElement, anchor: TranscriptScrollAnchor) {
	if (anchor.element?.isConnected && scroller.contains(anchor.element)) {
		scroller.scrollTop +=
			anchor.element.getBoundingClientRect().top - scroller.getBoundingClientRect().top - anchor.offset
	} else {
		scroller.scrollTop = anchor.top + scroller.scrollHeight - anchor.height
	}
}
