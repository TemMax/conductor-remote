import { readFileSync } from 'node:fs'
import { describe, expect, test } from 'vitest'
import { workspacesAny, workspacesRepos, workspacesState } from './workspaces.fixture.ts'

/**
 * The relay's workspace reads are checked twice against the same JSON: the Rust tests assert that
 * their output equals each file, and this test asserts that each file equals a constant the
 * compiler has checked against the web app's types. So a field the relay renames, drops or adds
 * fails one side or the other.
 */
function fixture(name: string): unknown {
	return JSON.parse(readFileSync(new URL(`./fixtures/${name}`, import.meta.url), 'utf8'))
}

describe('workspace reads contract', () => {
	test('GET /api/state workspaces are Workspace[]', () => {
		expect(fixture('workspaces-state.json')).toEqual(workspacesState)
	})

	test('GET /api/repos repos are RepoRow[]', () => {
		expect(fixture('workspaces-repos.json')).toEqual(workspacesRepos)
	})

	test('GET /api/workspaces/:id workspace is a SearchWorkspace', () => {
		expect(fixture('workspaces-any.json')).toEqual(workspacesAny)
	})
})
