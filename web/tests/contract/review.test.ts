import { readFileSync } from 'node:fs'
import { expect, test } from 'vitest'
import { reviewDiff } from './review.fixture.ts'

const FIXTURE = readFileSync(new URL('./fixtures/review-diff.json', import.meta.url), 'utf8')

test('the review diff fixture the relay is checked against is the typed one', () => {
	expect(JSON.parse(FIXTURE)).toStrictEqual(reviewDiff)
})
