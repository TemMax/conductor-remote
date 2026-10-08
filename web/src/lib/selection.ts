/**
 * What the phone's own text selection looks like from JavaScript.
 *
 * WebKit dispatches the same touch events whether a finger is scrolling the chat or
 * dragging a selection handle, so two controls at the edges of the screen used to read
 * one as the other. The message-nav pill woke on the `touchmove` a long press jitters
 * out and painted itself under the callout bar someone was reaching for; the drawer's
 * edge swipe read a handle drag near the left margin as a swipe, cancelled it with
 * `preventDefault` and slid the sidebar out. Both now ask here first.
 */

/** Is there live selected text anywhere on the page? */
export function hasSelection(): boolean {
	const sel = window.getSelection()
	return !!sel && !sel.isCollapsed && sel.toString().length > 0
}

/**
 * The handles hang above and below the selected text's own box, and grabbing one is
 * exactly the touch that must not become a swipe — so the test is generous.
 */
const HANDLE_SLACK = 28

/** Is this point on the selected text, or close enough to be one of its handles? */
export function overSelection(x: number, y: number): boolean {
	const sel = window.getSelection()
	if (!sel || sel.isCollapsed || !sel.rangeCount) return false
	for (const r of sel.getRangeAt(0).getClientRects()) {
		const near =
			x >= r.left - HANDLE_SLACK &&
			x <= r.right + HANDLE_SLACK &&
			y >= r.top - HANDLE_SLACK &&
			y <= r.bottom + HANDLE_SLACK
		if (near) return true
	}
	return false
}
