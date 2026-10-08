import { readFileSync } from 'node:fs'
import { expect, test } from 'vitest'
import { modelCatalog, modelDefaults } from './models.fixture.ts'

const CATALOG = readFileSync(new URL('./fixtures/models-catalog.json', import.meta.url), 'utf8')
const DEFAULTS = readFileSync(new URL('./fixtures/models-defaults.json', import.meta.url), 'utf8')

test('the model catalogue fixture the relay is checked against is the typed one', () => {
	expect(JSON.parse(CATALOG)).toStrictEqual(modelCatalog)
})

test('the model defaults fixture the relay is checked against is the typed one', () => {
	expect(JSON.parse(DEFAULTS)).toStrictEqual(modelDefaults)
})
