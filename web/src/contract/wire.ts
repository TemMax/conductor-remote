/**
 * The HTTP contract: every shape that crosses `/api`, in one place, named once.
 *
 * This file holds **no runtime code**. It re-exports the relay's own domain types
 * under the names the wire uses, and declares the response envelopes the route
 * handlers assemble. Three callers read it and none of them may disagree:
 *
 *   - `src/http/routes/` builds these payloads,
 *   - `web/src/lib/types.ts` re-exports the lot for the PWA,
 *   - `src/mcp/tools/` annotates its relay calls with them.
 *
 * Before this, the phone kept a hand-written mirror of all of it and `mcp-tools.ts`
 * kept a third copy inline. Nothing enforced the copies — a field renamed in
 * `reads.ts` typechecked cleanly on both sides and surfaced as `undefined` on a
 * phone. Each shape has one owner and is exported here; validated request types
 * are inferred from the Zod schemas in `src/contracts/agent-inputs.ts`.
 *
 * The web app may only `import type` from `src/` (see `scripts/check-imports.ts`).
 * `verbatimModuleSyntax` erases those imports, so nothing here reaches the bundle —
 * which is what lets a type live beside the `node:sqlite` code that produces it.
 * The one exception is `src/shared.ts`, which is stdlib-free on purpose.
 */

import type { DefaultEfforts } from './types/conductor-settings.ts'
import type { FirstPrompt } from './types/firstprompt.ts'
import type { CachedModelGroup } from './types/model-cache.ts'
import type { ParkedPrompt } from './types/parked.ts'

export type {
	AutoModelConfig,
	AutoModelConfigResponse,
	AutoModelProfile,
	AutoModelState,
	AutoModelTuple
} from './types/auto-model-types.ts'

import type { AGENT_EFFORTS } from './shared.ts'
import type { UpdateStatus } from './types/autoupdate.ts'
import type { ContextBreakdown } from './types/context-breakdown.ts'
import type { IndexStatus, SearchResult as SearchEvidence } from './types/coordinator.ts'
import type { DevServerForward, DevServerResult, DevServerState } from './types/dev-server-types.ts'
import type { LogEntry, LogFileInfo } from './types/logbuf.ts'
import type { NoSleepState } from './types/nosleep.ts'
import type { DeviceInfo } from './types/notify.ts'
import type { QuestionRequest, TranscriptEntry } from './types/parser.ts'

export type { QuestionAnswer, QuestionRequest } from './types/parser.ts'

import type {
	PlanUsageBucket,
	PlanUsageProviderId,
	PlanUsageSnapshot,
	PlanUsageWindow,
	ProviderPlanUsage
} from './types/plan-usage.ts'
import type { DraftAttachment, Prefs, SyncedDraft } from './types/prefs.ts'
import type {
	ClosedSession,
	Workspace as ReadWorkspace,
	RepoRow,
	SearchWorkspace,
	SessionRow
} from './types/reads-types.ts'
import type { DevRunConfig } from './types/run-configs.ts'
import type { Settings } from './types/settings.ts'
import type { ToolUsageSnapshot } from './types/tool-usage.ts'
import type { ActuatorInfo, SendResult as ActuatorSendResult } from './types/writes-types.ts'

export type { BackgroundTask } from './types/background-tasks.ts'
export type { SearchRole, SearchSnippet } from './types/coordinator.ts'
export type { DiffFile, DiffStats, WorkspaceDiff, WorkspaceFileDiff } from './types/diff.ts'
export type { RepoIcon } from './types/icons.ts'
export type { LogLevel } from './types/logbuf.ts'
export type { MergeMethod, MergeResult } from './types/merge.ts'
export type { NoSleepResult } from './types/nosleep.ts'
export type { PrStatus, UnreadSession } from './types/reads-types.ts'
export type {
	ActuatorInfo,
	CachedModelGroup,
	DefaultEfforts,
	DeviceInfo as PushDevice,
	DevRunConfig,
	DevServerForward,
	DevServerResult,
	DevServerState,
	DraftAttachment,
	IndexStatus as SearchIndexStatus,
	LogEntry,
	LogFileInfo,
	NoSleepState,
	PlanUsageBucket,
	PlanUsageProviderId,
	PlanUsageSnapshot,
	PlanUsageWindow,
	Prefs,
	ProviderPlanUsage,
	RepoRow as Repo,
	SearchWorkspace,
	SessionRow as Session,
	Settings as RelaySettings,
	SyncedDraft,
	TranscriptEntry,
	UpdateStatus
}

/** The live workspace as the relay's state endpoint returns it. */
export type Workspace = ReadWorkspace

// ── agent effort ────────────────────────────────────────────────────────────────

/** Stable effort keys accepted by the relay, independent of each provider's UI labels. */
export type AgentEffort = (typeof AGENT_EFFORTS)[number]

/**
 * A prompt the relay is holding and will deliver itself: a workspace's first prompt
 * waiting on setup (`src/delivery/firstprompt.ts`), or one parked for the lock screen
 * (`src/delivery/parked.ts`). Widened rather than a union, because the chat renders both with
 * one bubble and reads `reason` off either — the fields only a parked entry has are
 * optional here and required there.
 */
export type PendingPrompt = FirstPrompt & Partial<Omit<ParkedPrompt, keyof FirstPrompt>>

// ── envelopes ───────────────────────────────────────────────────────────────────
// The shapes that exist only as a response body. Everything above is a domain type
// the relay already had a name for.

/** GET /api/state — the sidebar, plus what the Connect sheet shows about this relay. */
export interface StateResponse {
	workspaces: Workspace[]
	/** Whether the Conductor app is running; absent from an older relay, which means running. */
	conductor?: { running: boolean }
	actuator: ActuatorInfo
	/** Relay version this daemon is running. */
	version?: string
	/** Self-update state (see src/host/autoupdate.ts). */
	update?: UpdateStatus
}

/** One workspace a search matched, with the chat evidence and the tab title to show. */
export interface SearchResult extends SearchEvidence<SearchWorkspace> {
	sessionTitle: string | null
}

/** GET /api/search?q=&repo=&archived=0 — name and transcript matches, merged and ranked. */
export interface SearchResponse {
	query: string
	/** Repo names the search was scoped to; empty means every repo. */
	repos: string[]
	index: IndexStatus
	results: SearchResult[]
}

/** GET /api/repos */
export interface ReposResponse {
	repos: RepoRow[]
}

/** GET /api/files/:reference — a bounded source preview for a chat file link. */
export interface FilePreviewResponse {
	/** Absolute workspace path from the link. */
	path: string
	/** The requested line, clamped to the file's last line. */
	line: number | null
	/** The first 1-based line included in `content`. */
	lineStart: number
	/** The last 1-based line included in `content`. */
	lineEnd: number
	totalLines: number
	/** Source text for the visible excerpt. */
	content: string
	/** The excerpt omits lines before or after it. */
	truncated: boolean
}

/**
 * GET /api/workspaces/:id/files — previewable, worktree-relative source paths.
 *
 * The phone uses these for the diff window's All-files rail and turns
 * `` `tests/foo.ts` `` in a message into a source link only when this list proves it
 * exists. `truncated` says the worktree held more previewable paths than the relay
 * will ship (src/git/diff.ts ▸ `listSourceFiles`).
 */
export interface WorkspaceFilesResponse {
	files: string[]
	truncated: boolean
}

/**
 * GET /api/workspaces/:id — one workspace by id, in any state.
 *
 * `SearchWorkspace`, not `Workspace`, and that is the whole point: this answers for the
 * archived ones too, which have no worktree, no live session and no git to read.
 */
export interface WorkspaceResponse {
	workspace: SearchWorkspace
}

/** GET /api/workspaces/:id/sessions */
export interface SessionsResponse {
	sessions: SessionRow[]
	/** Compact joins real Conductor chats into one tab in the PWA. Keyed by successor id. */
	chat_history?: Record<string, ChatHistoryLink>
}

export interface ChatHistoryLink {
	previousSessionId: string
	/** Keep the original conversation's label and position as new contexts are added. */
	title: string
	createdAt: string
}

/** GET /api/workspaces/:id/sessions/closed — fetched only while the picker is open. */
export interface ClosedSessionsResponse {
	sessions: ClosedSession[]
}

export type { ClosedSession }

/** GET /api/sessions/:id/messages?after= — `cursor` feeds the next poll. */
export interface MessagesResponse {
	pendingQuestion?: QuestionRequest
	/** Newly persisted transcript rows after the requested rowid cursor. */
	entries: TranscriptEntry[]
	/** Full current snapshot of Conductor's queue-mode message outbox. */
	queued?: TranscriptEntry[]
	cursor: number
}

/** GET /api/sessions/:id/context — exact total plus estimated composition and fork sizes. */
export type ContextBreakdownResponse = ContextBreakdown

/**
 * POST /api/sessions/:id/split — the source chat, written into a new destination's attachments.
 *
 * The destination is either another tab over the same files or a separate workspace
 * carrying a snapshot of the source's current files. It stops one step short of sending:
 * the prompt it composes goes through the ordinary send route for its retry, transcript
 * confirm and locked-Mac parking behavior.
 */
export interface SplitChatResult {
	ok: boolean
	/** Same-workspace tab, or a fresh worktree carrying the source's current code. */
	destination: 'chat' | 'workspace'
	/** The new chat. Present whenever the tab opened, even if nothing has been sent to it. */
	sessionId: string | null
	workspaceId: string
	/** Ready to POST to `sendPrompt`: the attachment token, then the caller's prompt. */
	text: string
	attachment: SplitAttachment
	error?: string
}

/** The transcript file `split` wrote, and what it left out of it. */
export interface SplitAttachment {
	name: string
	/** Worktree-relative — the path an agent should read, and what the token spells. */
	path: string
	bytes: number
	/** Transcript entries written. */
	kept: number
	/**
	 * Entries dropped, so a caller can report the cut instead of implying none. `thinking`
	 * and `tools` are the format's doing. `earlier`/`later` are the caller's: a
	 * `throughRowid` can leave later entries behind, while `onlyRowid` leaves both sides
	 * of its selected source message out. Both are zero when the whole chat was copied.
	 */
	elided: { thinking: number; tools: number; earlier: number; later: number }
}

/** POST /api/sessions/:id/agent — the chat is re-read from the DB before this answers. */
export interface AgentResult {
	ok: boolean
	session?: SessionRow
	error?: string
}

/** GET /api/sessions/:id/models — read live off Conductor's model menu, not a hard-coded list. */
export interface ModelsResult {
	ok: boolean
	models?: string[]
	/** The first model of Conductor's menu. */
	defaultModel?: string
	error?: string
}

/** GET /api/models — labels previously observed in Conductor's picker, without opening its UI. */
export interface ModelCatalogResponse {
	groups: CachedModelGroup[]
	/** The newest default observed in any live picker. */
	defaultModel?: string
}

/** GET/PATCH /api/models/defaults — Conductor's provider-specific new-chat effort defaults. */
export interface ModelDefaultsResponse {
	defaultEfforts: DefaultEfforts
}

/** GET /api/usage — provider subscription windows read from local agent CLIs. */
export type PlanUsageResponse = PlanUsageSnapshot

/** GET /api/usage/tools — recent tool traffic, separate from provider plan limits. */
export type ToolUsageResponse = ToolUsageSnapshot

/** Shared request inputs for staged agent settings and workspace creation. */
export type {
	AgentDraft as AgentPatch,
	CreateWorkspaceRequest,
	SetAgentOptionsRequest
} from './types/agent-inputs.ts'
export type { ToolUsageRange, ToolUsageRow } from './types/tool-usage.ts'

export interface CreateWorkspaceResult {
	ok: boolean
	workspaceId?: string
	workspace?: Workspace
	/** Echoed back so the caller can submit it once the worktree is ready. */
	pendingPrompt?: string
	/** The model requested for the new chat. The relay applies it before the first prompt. */
	model?: string
	sent?: boolean
	/** True when `send: true` waited for initial agent settings to finish. */
	configured?: boolean
	warning?: string
	error?: string
}

/** A file held by the relay until the new workspace has a worktree. */
export interface StagedAttachment extends Attachment {
	/** Opaque id accepted by `POST /api/workspaces` as an initial attachment. */
	stageId: string
}

/** POST /api/attachments — stage one phone file before its workspace exists. */
export interface StageAttachmentResult {
	ok: true
	attachment: StagedAttachment
}

/** POST /api/sessions/:id/prompt — the relay retries inside the request, hence `attempts`. */
export type { SendPromptRequest } from './types/agent-inputs.ts'

export interface SendResult extends ActuatorSendResult {
	/** Runs the relay needed to land the prompt (it retries a failed send itself). */
	attempts?: number
	/** The Mac is locked: the relay parked the prompt and delivers it on unlock. */
	parked?: boolean
	/** The parked entry, when `parked` — the same shape `/api/state` will carry. */
	queued?: PendingPrompt
	/** The chat the prompt landed in, when Conductor opened a new one for the chosen model. */
	sessionId?: string
}

/** One file written in Conductor's own attachment layout, ready to add to a prompt. */
export interface Attachment {
	/** Safe file name shown by Conductor's attachment chip. */
	name: string
	/** Worktree-relative path for the agent to read. */
	path: string
	bytes: number
	/** Conductor's `@⟦…⟧(…)` composer token. */
	token: string
}

/** POST /api/sessions/:id/attachments — a file from the phone, ready to send. */
export interface UploadAttachmentResult {
	ok: true
	attachment: Attachment
}

/** POST /api/sessions/:id/stop — Conductor's "Cancel agent" for one chat. */
export interface StopResult {
	ok: boolean
	/** The turn had already ended before the tap landed; nothing was pressed. */
	alreadyIdle?: boolean
	/** The chat as the relay re-read it once Conductor recorded the stop. */
	session?: SessionRow
	error?: string
}

/** DELETE /api/sessions/:id — Conductor's reversible "Close tab" action. */
export interface CloseChatResult {
	ok: boolean
	/** A retry named a session Conductor had already hidden. */
	alreadyClosed?: boolean
	/** The tab Conductor selected after closing, or null when none remain. */
	activeSessionId?: string | null
	/** The close was refused because this chat still has an agent running. */
	agentRunning?: boolean
	error?: string
}

/** POST /api/sessions/:id/restore — confirmed from Conductor's visible session rows. */
export interface RestoreChatResult {
	ok: boolean
	alreadyOpen?: boolean
	session?: SessionRow
	error?: string
}

/** POST /api/workspaces/:id/sessions — "New chat, same files". */
export interface NewChatResult {
	ok: boolean
	/** Id of the freshly-created session, if the relay detected it in time. */
	sessionId?: string | null
	error?: string
}

/** POST /api/workspaces/:id/continue — the same workspace and chats, now on a fresh branch. */
export interface ContinueWorkspaceResult {
	ok: boolean
	/** The branch Conductor moved away from. */
	previousBranch?: string
	/** Re-read only after Conductor recorded the new branch. */
	workspace?: Workspace
	error?: string
}

/** POST /api/workspaces/:id/status — the workspace re-read *after* Conductor recorded it. */
export interface StatusResult {
	ok: boolean
	workspace?: Workspace
	error?: string
}

/**
 * POST /api/workspaces/:id/archive — the workspace re-read once Conductor recorded it.
 *
 * `SearchWorkspace`, like the archived-chat read above and for the same reason: the
 * workspace this answers about no longer has a worktree, and its own success is what
 * took it off `/api/state`.
 */
export interface ArchiveResult {
	ok: boolean
	workspace?: SearchWorkspace
	/** It was already archived — a repeat of a request whose answer went missing, not a failure. */
	alreadyArchived?: boolean
	/** The refusal was "agents are still working here"; repeat with `stopAgents` to go ahead. */
	agentsRunning?: boolean
	error?: string
}

/** POST /api/conductor/restart */
export interface RestartConductorResult {
	ok: boolean
	/** How long the quit-and-relaunch took, once it got the UI lock. */
	ms?: number
	/**
	 * The refusal was "agents are still working" — repeat with `stopAgents` to end
	 * their turns and restart anyway. Same shape as `ArchiveResult.agentsRunning`,
	 * because it is the same question and the phone asks it the same way.
	 */
	agentsRunning?: boolean
	/** How many chats were mid-turn when it refused, so the dialog can name the cost. */
	working?: number
	error?: string
}

/** POST /api/conductor/launch */
export interface LaunchConductorResult {
	ok: boolean
	error?: string
}

/** GET /api/logs */
export interface LogsResponse {
	/** 'live' = this relay process's captured console; otherwise the log file that was tailed. */
	source: string
	/** False when the relay isn't the LaunchAgent — the files then belong to a *different* process. */
	managed: boolean
	startedAt: number
	/** Relay clock, so ages render right even if the phone's clock disagrees. */
	now: number
	files: LogFileInfo[]
	entries: LogEntry[]
}

/** GET /api/push */
export interface PushConfig {
	/** False when the relay was started with `PUSH_NOTIFY=off`. */
	enabled: boolean
	/** VAPID public key to subscribe with — stable for the life of the relay's store. */
	publicKey: string
	devices: DeviceInfo[]
}

/** POST /api/push/subscribe */
export interface PushSubscribeResult {
	ok: boolean
	/** This device's id, for `POST /api/push/test`. */
	id?: string
	devices: DeviceInfo[]
}

/** POST /api/push/test */
export interface PushTestResult {
	ok: boolean
	error?: string
}

/** GET /api/settings — the preferences plus what the phone needs to edit them. */
export interface SettingsResponse {
	settings: Settings
	nosleep: NoSleepStatus
	/** Is the lock screen up right now — `null` when the probe itself couldn't say. */
	screenLocked: boolean | null
}

/** GET/PATCH /api/prefs — the host's durable copy of local-first PWA state. */
export interface PrefsResponse {
	prefs: Prefs
}

/**
 * GET /api/nosleep — the window's state plus the ceiling a caller may ask for. It rides
 * along inside `SettingsResponse` as well, so the phone needs no second trip to draw the
 * control; both read this one declaration.
 */
export interface NoSleepStatus extends NoSleepState {
	maxSeconds: number
}
