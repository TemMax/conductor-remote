export interface DiffFile {
	/** Worktree-relative destination for a rename, otherwise the changed path. */
	path: string
	/** Original path when Git detects a rename or copy. */
	oldPath?: string
	added: number
	removed: number
}

/** Aggregate line changes for the compact workspace-sidebar readout. */
export interface DiffStats {
	added: number
	removed: number
}

export interface WorkspaceDiff {
	base: string
	mergeBase: string | null
	files: DiffFile[]
	patch: string
	truncated: boolean
	/** Uncommitted changes in the worktree (drives the "Commit & push" action). */
	dirty: boolean
	/** Commits on HEAD not yet on the remote-tracking branch (also drives "Commit & push"). */
	unpushed: boolean
}

/** One changed file's complete patch, fetched independently of the aggregate preview cap. */
export interface WorkspaceFileDiff {
	path: string
	patch: string
}
