/**
 * The relay's JSON shapes, as the PWA sees them.
 *
 * This used to be a hand-written mirror of `src/reads.ts`, `src/git.ts`, `src/search.ts`
 * and friends — 400-odd lines that nothing checked. A field renamed on the relay
 * typechecked cleanly on both sides and turned up as `undefined` on a phone, which is
 * the one failure a types file exists to prevent.
 *
 * So it is now a re-export of `src/wire.ts`, the relay's own declaration of what leaves
 * `/api`. `src/` and `web/src/` are one TypeScript project (see tsconfig.json), and
 * `verbatimModuleSyntax` erases a type-only import outright, so nothing from the relay
 * reaches the bundle — the `node:sqlite` code that produces these rows is one import
 * away and none of it is emitted.
 *
 * **Type-only, always.** A value import from `src/` would pull Node builtins into a
 * browser bundle; `scripts/check-imports.ts` fails the build if one appears. The single
 * module the web app may import a *value* from is `src/shared.ts`, which is stdlib-free
 * for exactly that reason.
 *
 * Keep this file a re-export. A shape that only the phone has (component props, local
 * drafts) belongs beside the component, and a shape the relay sends belongs in wire.ts.
 */

export type * from '../contract/wire.ts'
