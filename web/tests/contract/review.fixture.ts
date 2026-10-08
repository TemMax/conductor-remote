import type { WorkspaceDiff } from '../../src/contract/types/diff.ts'

/** What `workspace_diff` returns for the repository the golden test of `crates/relay/tests/review_diff.rs` builds. */
export const reviewDiff: WorkspaceDiff = {
	base: 'main',
	mergeBase: '3d58d88eb1fff597e9c97b8ca12ae83d28ebf9c3',
	files: [
		{
			path: 'keep.txt',
			added: 1,
			removed: 0
		},
		{
			path: 'src/search/coordinator.ts',
			oldPath: 'src/search.ts',
			added: 1,
			removed: 1
		},
		{
			path: 'notes.txt',
			added: 2,
			removed: 0
		}
	],
	patch:
		'diff --git a/keep.txt b/keep.txt\nindex 2fa992c..fe5841d 100644\n--- a/keep.txt\n+++ b/keep.txt\n@@ -1 +1,2 @@\n keep\n+more\ndiff --git a/src/search.ts b/src/search/coordinator.ts\nsimilarity index 94%\nrename from src/search.ts\nrename to src/search/coordinator.ts\nindex e8e92bd..c5542cc 100644\n--- a/src/search.ts\n+++ b/src/search/coordinator.ts\n@@ -8,7 +8,7 @@ const line6 = 6\n const line7 = 7\n const line8 = 8\n const line9 = 9\n-const line10 = 10\n+const line10 = 42\n const line11 = 11\n const line12 = 12\n const line13 = 13\ndiff --git a/notes.txt b/notes.txt\nnew file mode 100644\nindex 0000000..814f4a4\n--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n',
	truncated: false,
	dirty: true,
	unpushed: false
}
