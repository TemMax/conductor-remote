import { readFileSync } from 'node:fs'
import { describe, expect, test } from 'vitest'
import { closedSessions, sessions } from './sessions.fixture.ts'

function fixture(name: string): unknown {
	return JSON.parse(readFileSync(new URL(`./fixtures/${name}`, import.meta.url), 'utf8'))
}

/**
 * The relay's session reads are pinned by two golden files that its Rust tests compare with their own
 * output. These tests pin the same files to the web app's types, so a change to either side shows up here.
 */
describe('session fixtures', () => {
	test('sessions.json is the typed list of open chats', () => {
		expect(fixture('sessions.json')).toEqual(sessions)
	})

	test('sessions-closed.json is the typed list of closed chats', () => {
		expect(fixture('sessions-closed.json')).toEqual(closedSessions)
	})
})
