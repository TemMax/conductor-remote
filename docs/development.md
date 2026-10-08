# Development

## Prerequisites

- Rust, at the version pinned in `rust-toolchain.toml` (rustup installs it on first use).
- Node 24 or newer.
- macOS 14 or newer for bundling and the manual checks.

## Relay (`crates/relay`)

```sh
cargo build -p conductor-remote
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

### Conductor's files

The relay reads Conductor's SQLite database read-only and looks for its workspace checkouts on disk. Two environment variables move either; an empty value counts as unset.

- `CONDUCTOR_DB`: path of Conductor's database. Default: `~/Library/Application Support/com.conductor.app/conductor.db`.
- `CONDUCTOR_WORKSPACES`: the directory holding Conductor's workspace checkouts, as `<repository>/<workspace>`. Default: `~/conductor/workspaces`.

The database is opened on the first read after Conductor runs and closed when Conductor quits. While Conductor is not running the read routes answer 503 and `GET /api/state` answers without workspaces.

### Facts outside the database

Change statistics, pull-request state, Run tasks and the background tasks of a chat are not in Conductor's database. The relay calls three programs for them:

- `git`: added and removed lines of a worktree against its base branch, and whether an open pull request conflicts with its base.
- `gh`: the pull requests of a repository (`gh pr list`). Pull-request state needs a `gh` that is signed in; without one the workspaces simply show no pull request.
- `ps`: the process list, for the Run task of a worktree and for the agent process of a chat. Only the arguments of a process are requested, never its environment.

Nothing runs at start and no request waits for a program. A request returns what is known and queues a refresh on a pool of four background threads; the next request after the refresh finished shows the result, and the cached state and chat-list answers follow it. Each program is given 10 to 15 seconds and a bounded amount of output.

Lifetimes of what is known: change statistics 5 seconds for a workspace that is working and 60 seconds for any other (and again whenever the workspace changes); the pull requests of a repository 60 seconds; a conflict verdict 30 seconds; the process list 5 seconds; a chat-list answer at most 5 seconds. A Mac without `git` or `gh` shows no change statistics or pull-request state, and the routes answer as usual.

### Review and previews

Five read routes serve what a workspace changed and the files a chat links to. All are `GET`, need the token and answer 503 while Conductor is not running.

| Route | Answer |
| --- | --- |
| `/api/workspaces/:id/diff` | The workspace's changes against its target branch: base, merge base, changed files, patch, `truncated`, `dirty`, `unpushed`. 404 `workspace not found` for a workspace that is not ready or setting up; 409 `worktree path unresolved` when its worktree cannot be found. |
| `/api/workspaces/:id/diff/file?path=<p>` | The complete patch of one changed file. The same 404 and 409, then 400 `file path is required` when `path` is missing, empty or not decodable (`+` is a space, then percent-decoding), then 404 `changed file not found`. The first `path` parameter counts. |
| `/api/workspaces/:id/files` | The previewable source files of the worktree, at most 20,000 (`truncated` beyond). The same 404; an empty list without a worktree. |
| `/api/files/:reference` | A source file: `<absolute or ~/ path>[:line[:column]]`, percent-encoded as one segment. 404 `source file not found`, 403 with the reason, 413 `source file is too large to preview` (over 512 KiB), 415 `source file is not text`. |
| `/api/local-images/:path` | The bytes of a local image, with its content type and `cache-control: no-store`. 404 `image not found`, 413 `image is too large` (over 10 MiB). |

The review routes run `git` through the same command runner as the other background facts (so a test can script it), or through the real `git` when the relay has none. Nothing but `git` is run.

The file routes read only inside the preview roots, judged by real paths, so a link or `..` cannot leave them. The relay is reachable on the tailnet only (`EXPOSE=public` is refused at start), so the roots are the workspaces root, the home directory and Conductor's bundled skills (`/Applications/Conductor.app/Contents/Resources/conductor-skill/skills`). Local images may also come from the temporary directories (`$TMPDIR` and `/tmp`). A source file elsewhere answers 403 with a message that says why; an image elsewhere is a 404.

Without preview settings (a `Reads` built without `with_preview`) the two file routes answer 404.

### Search and usage

Three more read routes. All are `GET`, need the token and answer 503 `Conductor is not running` while Conductor is not running; each runs on the blocking pool.

| Route | Answer |
| --- | --- |
| `/api/search?q=&repo=&repo=&archived=0&limit=` | Workspaces matching `q` by name or by what was said in their chats. `repo` may repeat (repeated and empty values count once or not at all) and limits the search to those repo names. `archived=0` leaves archived workspaces out; any other value, or none, keeps them. `limit` is 12 when missing, not a number or zero, then at least 1 and at most 50, rounded down. Values are form-decoded (`+` is a space, then percent-decoding); an undecodable `q` counts as empty and an undecodable `repo` is dropped. |
| `/api/usage[?refresh=1]` | The plan allowances of Claude Code and Codex, kept for 60 seconds; `refresh=1` reads again. 503 `usage is not available` when the relay has no usage service. |
| `/api/usage/tools?range=24h\|7d\|30d[&refresh=1]` | The tool calls and their token weight in the saved chat traffic of that range (`24h` when `range` is missing), kept for 60 seconds per range. 400 `Choose 24h, 7d, or 30d for tool usage.` for any other range; 504 `Tool usage took too long to read. Try a shorter range.` after 60 seconds of scanning; 500 `internal error` when the scan fails; 503 `usage is not available` without the service. |

**The index.** Search over what was said in the chats reads a sidecar index, `search.db` in the state directory (`~/Library/Application Support/com.temmax.conductor-remote` unless `CONDUCTOR_REMOTE_STATE_DIR` moves it; the same directory as the token). It holds the prose of the chat messages, cut into chunks: what a person typed, what the assistant said and what it thought, but no tool traffic. It is not a copy of the database and can be deleted at any time: the next start builds it again. A file written by another schema version is dropped and rebuilt as well, and so is one that is ahead of Conductor's database (the database was replaced by a smaller one).

The index is filled by one thread, started with the relay:

- It runs while Conductor runs. It reads Conductor's database through a connection of its own, opened when Conductor starts and dropped when it quits, so a long first pass never holds up the phone's reads.
- It works in windows of 4,000 messages with a 5 millisecond pause between them, so the first pass over a large history is spread out. Until it has reached the newest message the answers carry `index.ready: false` and a `progress` between 0 and 1.
- Once caught up it looks at the database every 15 seconds and reads again only when the database changed.
- While Conductor is not running it does nothing and looks again every 15 seconds; search still answers, from what was indexed.
- When the relay shuts down, the server thread sets the stop flag and waits for the indexer before the process exits, so no write is cut off. A failed step is logged by kind (never a path or a message) and shown as `index.error`; the next pass, 15 seconds later, tries again.

If `search.db` cannot be opened the relay logs it and starts anyway: search then finds workspaces by name only, and `index.ready` stays `false`.

**The usage sheet** runs two programs, only when the sheet asks, and never with a prompt:

- `claude`: started as `claude -p --input-format stream-json --output-format stream-json --verbose --no-session-persistence --safe-mode` and asked for its usage through the control channel, with 10 seconds to answer.
- `codex`: started as `codex app-server` and asked for `account/rateLimits/read`, with 8 seconds to answer.

Each is looked for first in Conductor's own directory, as `~/Library/Application Support/com.conductor.app/agent-binaries/<program>/<version>/<program>` (the newest version wins), then on the `PATH`. A CLI that is not installed, does not answer or answers something unreadable shows as unavailable in its row; the other row is not affected. Cursor Agent and OpenCode expose no plan limits and are listed as such. The tool traffic needs no program: it is read from Conductor's database, one chat at a time, through a connection that is closed after each scan.

Tests never start either CLI: the plan service takes a `PlanProbe`, which tests replace with a fake that answers from a fixed value, and `SystemProbe::with_lookup(dir, false)` searches one directory only.

## Settings and logs

Every setting has one name, used both as the environment variable and as the key in `settings.json` in the state directory (`~/Library/Application Support/com.temmax.conductor-remote` unless `CONDUCTOR_REMOTE_STATE_DIR` moves it). A value comes from the environment first, then `settings.json`, then the default; an empty value counts as unset. The relay resolves the settings once, at start, and refuses to start when one is invalid.

`conductor-remote config` prints every setting with its value and where it came from (`environment`, `settings.json` or `default`), the state directory, the LaunchAgent's plist and whether the service is loaded, the tailnet URL when `tailscale serve` maps the relay's port, and the first 4 characters of the token followed by `…` (never the whole token).

`conductor-remote config set NAME VALUE` validates the value, writes it to `settings.json` (mode 0600, through a temporary file and a rename) and, when the service is loaded, restarts it with `launchctl kickstart -k` so it reads the new value. An empty value (`config set NAME ""`) removes the setting. An invalid value is refused with the rule it breaks, and nothing is written or restarted. The names and their rules:

| Name | Rule | Default |
| --- | --- | --- |
| `RELAY_PORT` | A whole number from 1 to 65535. The service takes its port from its plist, so run `service install` after changing it. | `8790` |
| `EXPOSE` | `tailnet` or `off`. `off` makes `service install` skip the `tailscale serve` mapping; `public` and `funnel` are refused. | `tailnet` |
| `PREVENT_SCREEN_LOCK` | `on` or `off`: whether keep-awake also keeps the display (and so the screen lock) from starting. | `off` |
| `PUSH_NOTIFY` | `on`, `off`, `true`, `false`, `1` or `0` (stored as `on` or `off`): whether the phone is notified. | `on` |
| `PUSH_SUBJECT` | Any text: the contact sent to the push services. | the project's URL |
| `CONDUCTOR_DB` | A path: Conductor's database. | `~/Library/Application Support/com.conductor.app/conductor.db` |
| `CONDUCTOR_WORKSPACES` | A path: Conductor's workspace checkouts. | `~/conductor/workspaces` |

`conductor-remote logs [-n LINES] [--no-follow]` runs `tail -n LINES -F` (default 100 lines; `--no-follow` leaves out `-F`) on the service's two log files. They live in `~/Library/Logs/com.temmax.conductor-remote`: `relay.log` (the relay's output) and `relay.err.log` (what it wrote to stderr, such as a panic). `service status` prints the same two paths, with the service's pid.

The phone reads the same log through `GET /api/logs`: without `file`, the last lines of the running relay (kept in memory, at most 600); with `file=relay.log` or `file=relay.err.log`, the end of that file. Every line passes through the redactor first, which masks the token, `sk-…` keys, `whsec_…` secrets and any `token=` value. `managed` is `true` when the relay runs as the LaunchAgent.

### Running under the menu-bar app

The menu-bar app starts the relay as a child process of its own and uses two commands:

```sh
conductor-remote start --exit-with-parent --parent-pid <the app's pid>
conductor-remote tailnet ensure --json
```

`start --exit-with-parent` runs the relay as `start` does and also stops it when the process that started it is gone. The relay notes its parent's process id at start and looks at it every second; once the id is another one (the system gave the relay a new parent), it logs `the process that started the relay is gone; stopping` and shuts down as it does on SIGTERM. It sends no signal to anything. Without the flag nothing watches the parent. `--parent-pid` (it needs `--exit-with-parent`) gives the app's own process id, which the relay compares against from the first look instead of the id it reads at start, so an app that died between starting the relay and that reading is noticed at once and not mistaken for the system's adopter.

`tailnet <status|ensure|off> [--json]` shows, makes or removes the `tailscale serve` mapping to the relay's port, the same mapping `service install` makes and `service uninstall` removes, without the LaunchAgent. It prints a few lines for a person, or with `--json` one line: `{"tailscale":bool,"host":string|null,"httpsPort":number|null,"mapped":bool,"url":string|null,"error":string|null}`. The exit status is 0 when `error` is `null` and 1 otherwise; the token is never printed.

`GET /api/host/status` (with the token) is what a local supervisor such as the app reads about the relay. It returns:

```json
{
  "version": "0.1.0",
  "pid": 4242,
  "startedAt": 1759824000000,
  "supervisor": "app",
  "port": 8790,
  "conductor": { "running": true },
  "accessibility": { "trusted": true },
  "screenLocked": false,
  "activity": { "working": 1, "idleMs": 5300 }
}
```

- `startedAt`: when the relay started, in milliseconds since the Unix epoch.
- `supervisor`: `app` for a relay started with `--exit-with-parent`, `launchd` for the LaunchAgent, `none` otherwise.
- `screenLocked`: `null` when that cannot be said.
- `activity.working`: the chats mid-turn (0 while Conductor is not running). `activity.idleMs`: how long ago a client last used the API, `null` before the first request. The status poll itself does not count.

### Cut-over checklist

Done together with the user, never by an agent alone:

1. Stop the old relay's login service and remove its tailnet mapping.
2. Set this relay to the old relay's port (`conductor-remote config set RELAY_PORT <port>`) and install it: `conductor-remote service install`.
3. Open the phone with the new URL and token that `service status` prints, and remove the old home-screen app.
4. Turn notifications on again on the phone: subscriptions belong to the old relay and do not carry over.

## Web app (`web/`)

```sh
npm --prefix web ci
npm --prefix web run build     # output in web/dist
npm --prefix web test
npm --prefix web run lint
```

## Bundling

`scripts/build-app.sh [--adhoc] [--skip-web] [--out DIR]` builds the web app, builds the release relay, builds the menu-bar app from `app/` (XcodeGen generates the Xcode project from `app/project.yml`; nothing generated is committed) and assembles `<DIR>/Conductor Remote.app` with the relay at `Contents/MacOS/conductor-remote`. It needs `xcodegen` on the `PATH` (`brew install xcodegen`).

- No `--adhoc`: signs with the first `Developer ID Application: ...` identity in the keychain (hardened runtime, secure timestamp) and fails if there is none, or if the built `Info.plist` has no `SUPublicEDKey`.
- `--adhoc`: ad-hoc signature, for local builds without a certificate. Accessibility grants do not survive rebuilds with it.
- `--skip-web`: skips `npm ci` and the web build. It requires an existing `web/dist`; the script fails if `web/dist/index.html` is missing.
- `--out DIR`: where the app is written. Default: `build/` (`build/` is not committed).

`scripts/check-app.sh` runs the tests of the app's package and builds the app target unsigned; CI runs it.

## Menu-bar app

The app in `app/` starts the relay, shows its state in the menu bar and updates itself with Sparkle.

- `scripts/build-app.sh [--adhoc] [--skip-web] [--out DIR]` builds and signs it with the relay inside; the flags are described under [Bundling](#bundling).
- `scripts/check-app.sh` runs the tests of `app/` and builds the app target unsigned. It starts neither the app nor the relay.
- Releases (signing, notarization, the update feed) are described in [releasing.md](releasing.md).

The app's own settings are not in `settings.json`; that file stays the relay's.

| Setting | Default | Stored in |
| --- | --- | --- |
| Launch at login | on: the first launch registers the app once | The system's login items (System Settings ▸ General ▸ Login Items). The app's defaults keep only `launchAtLoginRegisteredOnce`, the mark that the first launch has registered it. |
| Install updates automatically | on (`SUAutomaticallyUpdate` in the `Info.plist`) | `SUAutomaticallyUpdate` in the app's defaults. |

The app's defaults are the `com.temmax.conductor-remote` domain (`~/Library/Preferences/com.temmax.conductor-remote.plist`); `defaults read com.temmax.conductor-remote` prints them. Registering the login item fails for an app that is not in Applications, and the settings window says so.

Sparkle looks for an update once a day. With "Install updates automatically" on, it downloads the update in the background, and the app installs it only while the relay is idle: every 30 seconds it looks at `activity` in the relay's status and installs once no chat is mid-turn and no client has used the API for 10 minutes, or when the relay is not running. It does not install while the relay runs but does not answer its status: an unknown status is not idle. Sparkle then quits the app, which stops the relay, installs the update and starts the app again; the new app starts the new relay. An update that is still waiting when the app quits is installed at that quit. With the setting off, Sparkle asks before it downloads or installs anything.

## Checking Accessibility

`conductor-remote doctor` is a read-only check of what the relay can see of Conductor. It never sets an attribute, presses anything, types or activates an app.

1. Build the app with `scripts/build-app.sh` and copy `build/Conductor Remote.app` to Applications.
2. Grant `Conductor Remote` Accessibility in System Settings ▸ Privacy & Security ▸ Accessibility, or choose "Grant…" in the app's menu.
3. Read the result in the app's menu (the Accessibility row) or in the relay's `GET /api/host/status` (`accessibility.trusted`).

The app's main executable is the Swift app, which ignores command-line arguments, so the app cannot be asked to run `doctor`. `conductor-remote doctor` run from a terminal reports the terminal's own Accessibility grant, not the app's; use it to inspect Conductor's view with the terminal granted. `trusted` and `bundle_identifier` in its report are those of the process that ran it.

Options:

- `--prompt`: let macOS show its Accessibility dialog when the app is not trusted yet.
- `--tree`: include a snapshot of Conductor's element tree. It needs the grant and a running Conductor.
- `--locate`: find the pane header, the chat tabs and the composer the writes use, and say what was found (`view` in the JSON, left out when the view was not read). It reads only. It needs the grant and a running Conductor.
- `--depth N` (default 30) and `--max-nodes N` (default 5000): how much of the tree is read.
- `--values N`: keep the first N characters of each value. Values are hidden without it. Titles, descriptions, help and placeholders are never hidden: they are the labels the probe is for.
- `--out PATH`: also write the report as JSON to PATH. Use an absolute path. An existing file is removed first and the new one is created with mode 0600.

The summary lists the relay's executable and bundle identifier, whether the grant is held, whether Conductor runs (and its process id), the session state (locked, on console, or unknown) and the titles of Conductor's windows. Exit status: 0 when it reported, 3 when `--tree` or `--locate` was asked but nothing was read (the app is not trusted, Conductor is not running, or the view could not be read), 4 when `--out` could not be written (2 is a usage error).

The tree file holds window titles and element labels, which can include workspace and chat names. Delete it when you are done and do not share it unreviewed.

## Manual checks

1. Build and sign: `scripts/build-app.sh`. Confirm `codesign -dv` shows the Developer ID team.
2. Grant Accessibility to `Conductor Remote` in System Settings ▸ Privacy & Security ▸ Accessibility.
3. Rebuild with `scripts/build-app.sh` and confirm the grant is still listed and still enabled — a stable signing identity is what keeps it.
4. Start the relay, quit Conductor, and confirm the phone shows "Conductor is not running"; press "Launch Conductor" and confirm it opens and the phone leaves that state without a reload.

## Comparing two relays

`scripts/parity.py` (Python 3, standard library only) requests the read endpoints of two relays and compares the JSON structurally: `GET /api/state`, `GET /api/workspaces/<id>/sessions` for the first workspaces of A's answer and `GET /api/sessions/<id>/messages?after=0` for the first chats of each. Redirects are not followed, so a token never goes to another host.

Environment variables (all four are required; tokens are never passed on the command line):

- `PARITY_A_URL`, `PARITY_B_URL`: base URLs, for example `http://127.0.0.1:8790`. They must be `http` or `https` URLs with a host.
- `PARITY_A_TOKEN_FILE`, `PARITY_B_TOKEN_FILE`: paths of files holding each relay's bearer token. The file must hold a single line of printable ASCII without spaces (1 to 512 bytes, one trailing newline allowed); anything else is refused before any request is made.

```sh
PARITY_A_URL=http://127.0.0.1:8790 PARITY_A_TOKEN_FILE=./a.token \
PARITY_B_URL=http://127.0.0.1:8791 PARITY_B_TOKEN_FILE=./b.token \
python3 scripts/parity.py --workspaces 3 --sessions 2 --ignore 'workspaces[].change_stats'
```

Options:

- `--workspaces N`: workspaces to descend into (default 5; 0 descends into none).
- `--sessions N`: chats per workspace (default 2).
- `--ignore PATH`: a JSON path pattern to skip, with everything under it, for example `workspaces[].change_stats`. Repeatable.
- `--self-test`: runs the built-in assertions without network and without reading the environment.

Exit status: 0 when everything is identical, 1 when anything differs, 2 on a usage, token-file, URL or connection error (or any unexpected error).

A request counts as identical only when both relays answered 200 with a JSON body and no path differs. A different status adds `<status>`, a body that is not JSON adds `<body>`. When `/api/state` is not a 200 JSON answer on both sides the run stops after that line.

The script prints paths, types, string lengths, counts, status codes and exception class names only. It never prints a value from a response, a token, a byte of a token file or the message text of an exception. An object key is printed only when it is a plain name (`[A-Za-z_][A-Za-z0-9_]{0,39}`); any other key is printed as `<key>`.

## Public source checks

Before committing or pushing, run `bash scripts/check-public-files.sh` and review
the staged filenames and diff. Private agent instructions, planning documents,
local credentials and runtime data must remain outside tracked Git files.

Scan a clean checkout with Gitleaks 8.30.1 or later:

```sh
gitleaks git . --log-opts=--all --redact=100
```

The configuration allows only the exact public Sparkle key and reviewed test
vectors in their specific source files. New findings require review; do not
exclude entire test directories or print credential values in reports.
