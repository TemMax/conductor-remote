import { describe, expect, test } from 'vitest'
import { type NewWorkspaceCreateState, newWorkspaceDisabledReason } from '../../src/lib/prompts/new-workspace.ts'

const ready: NewWorkspaceCreateState = {
	online: true,
	repoStatus: 'selected',
	uploading: false,
	attachmentError: false,
	hasInitialPrompt: false
}

describe('new workspace create availability', () => {
	test('leaves an ordinary empty workspace ready to create', () => {
		expect(newWorkspaceDisabledReason(ready)).toBeNull()
	})

	test.each([
		[{ online: false }, 'Reconnect to the relay to continue.'],
		[{ repoStatus: 'loading' }, 'Loading repositories…'],
		[{ repoStatus: 'error' }, 'Couldn’t load repositories.'],
		[{ repoStatus: 'empty' }, 'No Conductor repositories are available.'],
		[{ repoStatus: 'required' }, 'Choose a repository to continue.'],
		[{ uploading: true }, 'Wait for attachments to finish uploading.'],
		[{ attachmentError: true }, 'Remove failed attachments to continue.']
	] as const)('names the blocker for %j', (patch, reason) => {
		expect(newWorkspaceDisabledReason({ ...ready, ...patch })).toBe(reason)
	})

	test('shows the first actionable blocker when more than one applies', () => {
		expect(
			newWorkspaceDisabledReason({
				...ready,
				online: false,
				repoStatus: 'loading',
				uploading: true
			})
		).toBe('Reconnect to the relay to continue.')
	})
})
