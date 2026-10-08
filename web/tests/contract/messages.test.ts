import { readFileSync } from 'node:fs'
import { expect, test } from 'vitest'
import { messages } from './messages.fixture.ts'

const FIXTURE = readFileSync(new URL('./fixtures/messages.json', import.meta.url), 'utf8')

test('the messages fixture the relay is checked against is the typed one', () => {
	expect(JSON.parse(FIXTURE)).toStrictEqual(messages)
})
