/**
 * How the phone should render a repo's sidebar avatar. `emoji`/`named` render
 * inline (no bytes fetched); `file` is served by `/api/repos/:name/icon`;
 * `github` is loaded straight from `github.com/<owner>.png`. Null → monogram.
 */
export type RepoIcon =
	| { kind: 'emoji'; value: string }
	| { kind: 'named'; value: string }
	| { kind: 'file' }
	| { kind: 'github'; owner: string }
