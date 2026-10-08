import type { ModelCatalogResponse, ModelDefaultsResponse } from '../../src/contract/wire.ts'

/**
 * What `GET /api/models` returns for the catalogue file the relay's test writes: three pickers, labels
 * cleaned and ordered, the default of the picker updated last. The relay's test produces the same value
 * and compares it with `fixtures/models-catalog.json`, which the test beside this file holds equal to
 * this constant.
 */
export const modelCatalog: ModelCatalogResponse = {
	groups: [
		{
			agentType: 'claude',
			models: ['5.6 Sol', '5.6 Terra', 'Fable 5.1'],
			defaultModel: '5.6 Sol',
			snapshotAt: 1760000000000,
			snapshotModels: ['5.6 Sol', 'Fable 5.1'],
			selections: [{ model: '5.6 Sol', selectedAt: 1760000000500 }],
			updatedAt: 1760000001000
		},
		{
			agentType: 'codex',
			models: ['5.6 Terra'],
			defaultModel: '5.6 Terra',
			snapshotAt: null,
			updatedAt: 1760000002000
		},
		{
			agentType: 'unknown',
			models: ['opencode-go/muse-spark'],
			updatedAt: 0
		}
	],
	defaultModel: '5.6 Terra'
}

/** What `GET /api/models/defaults` returns for the settings file the relay's test writes; see `fixtures/models-defaults.json`. */
export const modelDefaults: ModelDefaultsResponse = {
	defaultEfforts: { claude: 'max', codex: 'xhigh' }
}
