import { readFileSync } from 'node:fs'
import { expect, test } from 'vitest'
import { contextBreakdown } from './context.fixture.ts'

const FIXTURE = readFileSync(new URL('./fixtures/context-breakdown.json', import.meta.url), 'utf8')

test('the context fixture the relay is checked against is the typed one', () => {
	expect(JSON.parse(FIXTURE)).toStrictEqual(contextBreakdown)
})
