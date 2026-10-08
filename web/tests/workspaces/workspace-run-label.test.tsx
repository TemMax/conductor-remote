import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, test } from 'vitest'
import { WorkspaceRunLabel } from '../../src/components/workspaces/WorkspaceRunLabel.tsx'

const modelGroups = [
	{
		agentType: 'claude',
		models: ['Fable 5.1'],
		updatedAt: 1
	}
]

const ordinary = {
	agent_type: 'claude',
	model: 'fable-5-1'
}

describe('workspace sidebar run label', () => {
	test('keeps the active model for an ordinary workspace', () => {
		const html = renderToStaticMarkup(<WorkspaceRunLabel workspace={ordinary} modelGroups={modelGroups} />)

		expect(html).toContain('Fable 5.1')
	})
})
