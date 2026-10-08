import type { SessionRow, Workspace } from '../../src/contract/types/reads-types.ts'

/** What `Reads::list_workspaces` returns, background facts filled, for the seed of `crates/relay/tests/support/seed_extras.rs`. */
export const workspacesExtras: Workspace[] = [
	{
		id: 'ext-workspace',
		directory_name: 'ext-dir',
		workspace_name: 'Extras ext-draft',
		branch: 'ext-draft',
		pr_title: null,
		derived_status: 'in-progress',
		manual_status: null,
		state: 'ready',
		created_at: '2026-03-01 08:00:00',
		updated_at: '2026-03-01 10:00:00',
		pinned_at: null,
		active_session_id: '0e57ce11-0000-4000-8000-000000000001',
		intended_target_branch: null,
		repo_name: 'ext-repo',
		repo_root: '/ext/repos/ext-repo',
		repo_icon: null,
		remote_url: 'https://github.com/ext-owner/ext-repo.git',
		default_branch: 'main',
		session_status: 'idle',
		session_title: 'Wait for the build',
		model: null,
		agent_type: 'claude',
		unread_sessions: [],
		worktree: '/ROOT/ext-repo/ext-dir',
		baseBranch: 'main',
		icon: {
			kind: 'github',
			owner: 'ext-owner'
		},
		change_stats: {
			added: 13,
			removed: 3
		},
		pr_status: 'draft',
		pr_number: 11,
		pr_url: 'https://example.test/pull/11',
		run_active: true
	},
	{
		id: 'ext-workspace-bare',
		directory_name: 'ext-bare',
		workspace_name: 'Extras ext-merged',
		branch: 'ext-merged',
		pr_title: null,
		derived_status: 'in-progress',
		manual_status: null,
		state: 'ready',
		created_at: '2026-03-01 08:00:00',
		updated_at: '2026-03-01 09:00:00',
		pinned_at: null,
		active_session_id: null,
		intended_target_branch: null,
		repo_name: 'ext-repo',
		repo_root: '/ext/repos/ext-repo',
		repo_icon: null,
		remote_url: 'https://github.com/ext-owner/ext-repo.git',
		default_branch: 'main',
		session_status: null,
		session_title: null,
		model: null,
		agent_type: null,
		unread_sessions: [],
		worktree: null,
		baseBranch: 'main',
		icon: {
			kind: 'github',
			owner: 'ext-owner'
		},
		change_stats: null,
		pr_status: 'merged',
		pr_number: 12,
		pr_url: 'https://example.test/pull/12',
		run_active: false
	}
]

/** What `Reads::list_sessions` returns for `ext-workspace`, once its chat's agent process has been listed. */
export const sessionsBackground: SessionRow[] = [
	{
		id: '0e57ce11-0000-4000-8000-000000000001',
		status: 'idle',
		title: 'Wait for the build',
		model: null,
		permission_mode: 'default',
		claude_effort_level: null,
		fast_mode: 0,
		agent_type: 'claude',
		context_used_percent: null,
		unread_count: 0,
		created_at: '2026-03-01 08:00:00',
		updated_at: '2026-03-01 08:00:00',
		last_user_message_at: null,
		prompt_cache_ttl_ms: null,
		turn_started_at: null,
		background_tasks: [
			{
				taskId: 'task-open',
				toolUseId: 'toolu_task-open',
				description: 'Watch the build',
				taskType: 'local_bash',
				since: '2099-01-01 00:00:01'
			}
		]
	},
	{
		id: 'ext-chat-idle',
		status: 'idle',
		title: 'Nothing running',
		model: null,
		permission_mode: 'default',
		claude_effort_level: null,
		fast_mode: 0,
		agent_type: 'claude',
		context_used_percent: null,
		unread_count: 0,
		created_at: '2026-03-01 08:30:00',
		updated_at: '2026-03-01 08:30:00',
		last_user_message_at: null,
		prompt_cache_ttl_ms: null,
		turn_started_at: null,
		background_tasks: []
	}
]
