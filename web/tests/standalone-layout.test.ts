import { readFileSync } from 'node:fs'
import { runInNewContext } from 'node:vm'
import { describe, expect, it } from 'vitest'

const headScript = readFileSync(new URL('../index.html', import.meta.url), 'utf8').match(
	/<script>([\s\S]*?)<\/script>/
)?.[1]
if (!headScript) throw new Error('Missing startup script')

function boot(standalone?: boolean, storageBlocked = false) {
	const dataset: Record<string, string> = {}
	runInNewContext(headScript as string, {
		navigator: { standalone },
		localStorage: {
			getItem: () => {
				if (storageBlocked) throw new Error('Storage unavailable')
				return null
			}
		},
		window: { matchMedia: () => ({ matches: false }) },
		document: { documentElement: { dataset }, querySelector: () => ({ content: '' }) }
	})
	return dataset
}

describe('installed iOS layout before first paint', () => {
	it('marks an iOS home-screen app without depending on Safari version tokens', () => {
		expect(boot(true)).toHaveProperty('iosStandalone')
	})

	it('still marks the installed app when theme storage is unavailable', () => {
		expect(boot(true, true)).toHaveProperty('iosStandalone')
	})

	it('keeps Safari tabs and browsers without the iOS standalone property unmarked', () => {
		expect(boot(false)).not.toHaveProperty('iosStandalone')
		expect(boot()).not.toHaveProperty('iosStandalone')
	})
})
