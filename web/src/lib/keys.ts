/**
 * Does this Enter mean "submit"?
 *
 * With a hardware keyboard the convention holds: Enter submits, Shift+Enter breaks
 * the line. A phone's keyboard has no Shift+Enter — its return key is the only way
 * to break a line at all — so on a touch device Enter has to insert the newline and
 * the Send button is how a prompt goes. Which device this is comes from the primary
 * pointer (`pointer: coarse` is touch), read at the keypress rather than once: an
 * iPad grows a fine pointer the moment a trackpad connects, and Enter should follow.
 *
 * Cmd/Ctrl+Enter submits everywhere. It is the one chord a hardware keyboard paired
 * with a phone still has, and on the chat composer it keeps Conductor's own meaning
 * (queue behind the running answer) — the caller reads the modifier itself.
 */
export type EnterKeyEvent = {
	key: string
	shiftKey: boolean
	metaKey: boolean
	ctrlKey: boolean
	nativeEvent: { isComposing: boolean }
}

/** The primary pointer is a finger, so the keyboard on screen has no Shift+Enter. */
export const touchKeyboard = (): boolean =>
	typeof window !== 'undefined' && window.matchMedia('(pointer: coarse)').matches

export function enterSubmits(e: EnterKeyEvent, touch: boolean = touchKeyboard()): boolean {
	// An IME's own Enter picks a candidate; it is not ours to act on.
	if (e.key !== 'Enter' || e.nativeEvent.isComposing || e.shiftKey) return false
	if (e.metaKey || e.ctrlKey) return true
	return !touch
}
