import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, test, vi } from 'vitest'

Object.defineProperty(globalThis, 'location', { configurable: true, value: { hash: '', pathname: '/', search: '' } })
Object.defineProperty(globalThis, 'localStorage', {
	configurable: true,
	value: { getItem: () => null, setItem: () => {}, removeItem: () => {} }
})
Object.defineProperty(globalThis, 'history', { configurable: true, value: { replaceState: () => {} } })

const { RoleEditorCard, roleModelProblem, roleWithModel } = await import(
	'../../src/components/agents/RoleEditorCard.tsx'
)

describe('role editor card', () => {
	test('role editor leaves an unavailable model visibly invalid and offers no Plan control', () => {
		const html = renderToStaticMarkup(
			<RoleEditorCard
				name="exploration"
				role={{ model: 'Muse Spark', effort: 'high', fast: false }}
				models={['Fable 5', '5.6 Terra']}
				invalid="Choose an exact model from Conductor’s picker."
				onChange={vi.fn()}
				onRemove={vi.fn()}
				canRemove
			/>
		)
		expect(html).toContain('Muse Spark')
		expect(html).toContain('Choose an exact model')
		expect(html).not.toContain('Plan mode')
	})

	test('uses every saved picker label for role editing and validation', () => {
		const currentModels = ['5.6 Sol', 'opencode-go/muse-spark-1.3-contributor']
		const groups = [
			{ agentType: 'claude', models: ['Fable 5'], snapshotAt: 0, updatedAt: 0 },
			{ agentType: 'codex', models: ['Fable 5.1'], snapshotAt: 1, updatedAt: 1 },
			{ agentType: 'codex', models: currentModels, snapshotAt: 2, updatedAt: 2 }
		]

		expect(roleModelProblem({ model: 'Fable 5.1' }, groups)).toBeNull()
		expect(roleModelProblem({ model: 'Fable 5' }, groups)).toBeNull()
		expect(roleModelProblem({ model: 'unknown-model' }, groups)).toContain('exact model')
	})

	test('hides unsupported OpenCode controls and drops them when its model is selected', () => {
		const model = 'opencode-go/muse-spark-1.3-contributor'
		const role = { model, effort: 'high' as const, fast: false }
		const html = renderToStaticMarkup(
			<RoleEditorCard
				name="exploration"
				role={role}
				models={[model]}
				agentType="acp"
				onChange={vi.fn()}
				onRemove={vi.fn()}
				canRemove
			/>
		)

		expect(html).not.toContain('Reasoning effort for exploration')
		expect(html).not.toContain('Fast mode for exploration')
		expect(roleModelProblem(role, [{ agentType: 'acp', models: [model], updatedAt: 1 }])).toContain(
			'does not expose a reasoning control'
		)
		expect(roleWithModel(role, model)).toEqual({ model })
	})
})
