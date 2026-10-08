import { readFileSync } from 'node:fs'
import { expect, test } from 'vitest'
import { transcriptEntries } from './transcript.fixture.ts'

const FIXTURE = readFileSync(new URL('./fixtures/transcript-entries.json', import.meta.url), 'utf8')

test('the transcript fixture the relay is checked against is the typed one', () => {
	expect(JSON.parse(FIXTURE)).toStrictEqual(transcriptEntries)
})
