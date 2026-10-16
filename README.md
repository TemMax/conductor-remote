# Conductor Remote

A phone control panel for your local Conductor agents on macOS: watch every workspace, read
live transcripts, review diffs and send prompts from your phone, over your tailnet.

Claude and Codex interactive questions appear as cards in the chat. Choose options or write
your own answers, then submit the whole set together. Answering in Conductor or Remote closes
the card and preserves it in the transcript.

Open saved `.html`/`.htm` files from chat or Review's All-files view as interactive
previews, with neighboring CSS, images, fonts, modules and JSON. HTML code fences
have a Preview button. The shared viewer includes full original Code, Refresh and
diagnostics; explicit line links start in Code. Previews use an isolated browser
origin and ordinary browser rules for CDN resources and external APIs. Projects
that need a build or backend still use Run. See [preview behavior and limits](docs/development.md#html-artifact-previews).

Two parts:

- **The relay** (`crates/relay`, Rust) runs on the Mac next to Conductor. It reads Conductor's
  state, drives its window and serves the web app. It is active while Conductor is running and
  dormant otherwise.
- **The web app** (`web/`, React) is what the phone opens.

Status: early development. See [development instructions](docs/development.md) for building and testing.

## Install

1. Download `Conductor-Remote-<version>.dmg` from the
   [latest release](https://github.com/TemMax/conductor-remote/releases/latest).
2. Open it and drag Conductor Remote to Applications.
3. Open the app and follow the first-run window.

Requirements: macOS 14 or later on Apple Silicon, Conductor, and Tailscale for the phone link.

## Development

The web toolchain uses Bun 1.3.12 (pinned in `web/package.json`); Node is not required.

    cargo test --workspace
    bun install --cwd web --frozen-lockfile
    bun run --cwd web verify
    bun run --cwd web build

## License

MIT — see [LICENSE](LICENSE).

## Acknowledgements

Inspired by, and with big thanks to, [hyldmo/conductor-remote](https://github.com/hyldmo/conductor-remote).

The web app in `web/` and the assets in `public/` are derived from that project, which is
distributed under the MIT License:

> MIT License
>
> Copyright (c) 2026 Eivind Hyldmo
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all
> copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.
