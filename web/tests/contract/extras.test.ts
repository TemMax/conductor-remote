import { readFileSync } from 'node:fs'
import { describe, expect, test } from 'vitest'
import { sessionsBackground, workspacesExtras } from './extras.fixture.ts'

function fixture(name: string): unknown {
	return JSON.parse(readFileSync(new URL(`./fixtures/${name}`, import.meta.url), 'utf8'))
}

/**
 * The relay's reads with background facts are pinned by two golden files that its Rust tests compare with
 * their own output. These tests pin the same files to the web app's types.
 */
describe('extras fixtures', () => {
	test('workspaces-extras.json is the typed workspace list with the background facts filled', () => {
		expect(fixture('workspaces-extras.json')).toEqual(workspacesExtras)
	})

	test('sessions-background.json is the typed chat list with background tasks', () => {
		expect(fixture('sessions-background.json')).toEqual(sessionsBackground)
	})
})
