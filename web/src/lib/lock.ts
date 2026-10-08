import { isLockedError } from '../contract/shared.ts'

export { isLockedError }

/**
 * Screen Sharing on the relay's Mac, addressed the way this phone already reaches it: the PWA is
 * served from that Mac's own MagicDNS name, so `location.hostname` is the host to unlock and no
 * relay round trip is needed to learn it. `vnc://` is the scheme the iOS clients register (Screens,
 * RealVNC, Jump), and it is the only remote path there is: macOS exposes no unlock API, the lock
 * screen refuses synthetic keystrokes, and Apple's own Screen Sharing server is the one input
 * channel it still accepts. So the link carries you to the password prompt; it doesn't answer it.
 * Loopback returns null because a dev checkout serves this same UI from 127.0.0.1.
 */
export function unlockUrl(): string | null {
	const host = location.hostname
	if (!host || host === 'localhost' || host.startsWith('127.')) return null
	return `vnc://${host}`
}
