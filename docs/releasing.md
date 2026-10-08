# Releasing

A release is a signed and notarized `Conductor-Remote-<version>.dmg`, the update feed `appcast.xml` and `SHA256SUMS`, attached to a GitHub release. `.github/workflows/release.yml` builds all three from a tag.

## One-time setup

The workflow reads six repository secrets:

| Secret | Holds |
| --- | --- |
| `BUILD_CERTIFICATE_BASE64` | The `Developer ID Application` certificate with its private key, exported as a `.p12` file and base64-encoded. |
| `P12_PASSWORD` | The password the `.p12` was exported with. |
| `NOTARY_KEY_BASE64` | The App Store Connect API key used for notarization, the `.p8` file, base64-encoded. |
| `NOTARY_KEY_ID` | The key ID of that API key. |
| `NOTARY_ISSUER_ID` | The issuer ID of that API key. |
| `SPARKLE_PRIVATE_KEY` | The private key that signs updates: the content of the `generate_keys -x` file (one base64 line, the 32-byte seed). |

To enter them one at a time in a terminal, authenticate with GitHub CLI and run:

```sh
gh auth login
zsh -f scripts/setup-release-secrets.zsh TemMax/conductor-remote
```

On macOS, use clipboard mode for values copied from 1Password, especially long
certificate strings that exceed the terminal's line-input buffer:

```sh
zsh -f scripts/setup-release-secrets.zsh --clipboard TemMax/conductor-remote
```

For each prompt, copy that secret in 1Password, return to the terminal and press
Enter without pasting. The script reads the clipboard at that point, without
displaying its content. Wrapped base64 is accepted in this mode. The script does
not change the clipboard; the password manager's clipboard expiry still applies.

The script hides input and passes values to `gh secret set` through stdin, without
putting them in command arguments or writing temporary files. Press Enter after
each value. For the certificate and notarization key, paste one base64 line or
enter `@/path/to/file.p12` / `@/path/to/AuthKey.p8`; spaces in paths are allowed
without quotes. Existing secrets are replaced. If a save fails, the script stops;
secrets already saved remain, and rerunning safely replaces them again. Keep the
existing Sparkle key so installed copies continue to trust updates.

Alternatively, copy each from 1Password without printing it. Replace `<vault>`, `<item>` and `<field>` with where the value is kept:

```sh
op read "op://<vault>/<item>/<field>" | gh secret set BUILD_CERTIFICATE_BASE64 -R TemMax/conductor-remote
op read "op://<vault>/<item>/<field>" | gh secret set P12_PASSWORD -R TemMax/conductor-remote
op read "op://<vault>/<item>/<field>" | gh secret set NOTARY_KEY_BASE64 -R TemMax/conductor-remote
op read "op://<vault>/<item>/<field>" | gh secret set NOTARY_KEY_ID -R TemMax/conductor-remote
op read "op://<vault>/<item>/<field>" | gh secret set NOTARY_ISSUER_ID -R TemMax/conductor-remote
op read "op://<vault>/<item>/<field>" | gh secret set SPARKLE_PRIVATE_KEY -R TemMax/conductor-remote
```

The value of `BUILD_CERTIFICATE_BASE64` and of `NOTARY_KEY_BASE64` is the base64 of the file, not the file. When 1Password keeps the file itself, encode it on the way:

```sh
base64 -i file.p12 | gh secret set BUILD_CERTIFICATE_BASE64 -R TemMax/conductor-remote
base64 -i AuthKey.p8 | gh secret set NOTARY_KEY_BASE64 -R TemMax/conductor-remote
```

The Sparkle key is this app's own: account `conductor-remote` in the login keychain, not Sparkle's default account. `generate_keys --account conductor-remote -x <file>` writes it to a file, and that file's content is `SPARKLE_PRIVATE_KEY`; delete the file once the secret and the 1Password copy are set. Its public half is `SUPublicEDKey` in `app/project.yml`. Keep the key safe: installed copies accept only updates signed with it, so losing it means they can no longer update.

## Cutting a release

1. Bump the Cargo workspace version (`version` under `[workspace.package]` in `Cargo.toml`) and `MARKETING_VERSION` in `app/project.yml` together, to the same `X.Y.Z`.
2. Raise `CURRENT_PROJECT_VERSION` in `app/project.yml` by one.
3. Write `docs/release-notes/vX.Y.Z.md`. It becomes the text of the GitHub release and of the update window.
4. Check: `bash scripts/check-release-version.sh vX.Y.Z`.
5. Merge to `main`.
6. Tag `main` and push the tag:

   ```sh
   git tag -a vX.Y.Z -m "Conductor Remote X.Y.Z"
   git push origin vX.Y.Z
   ```

The tag push starts the workflow, which publishes the release.

To check signing and notarization first, start the workflow by hand (Actions ▸ release ▸ Run workflow) on `main` and leave `publish` off. It does everything but publish, and uploads the DMG, `appcast.xml` and `SHA256SUMS` to the run. Every release build requires a commit on `origin/main` before signing secrets are used; publishing also requires a tag. On `main` the version is the Cargo workspace version.

The build number must be above the latest release's. Re-running a tag that was already released, or a manual check run after a release without a bump, stops at the version check.

## Updating GitHub Actions

Keep every `uses:` pinned to a full commit SHA, with the corresponding release
version in a comment. To update an action, inspect its upstream release and
resolve that release's tag to a commit in the official repository. Review the
changes, update the SHA and version comment together, then run `actionlint` and
CI before merging. Do not replace the SHA with a moving tag.

## What the workflow does

1. Checks out the repository, rejects tracked private working files and settles the tag: the tag the run is on, or `v` + the Cargo workspace version on `main`. Every run stops unless its commit is on `origin/main`, before any signing secret is used. A publishing run also requires a tag.
2. Checks for Xcode 26 and installs Node 24, XcodeGen 2.46.0 and the Rust toolchain of `rust-toolchain.toml`.
3. Reads the build number of the latest release from its `appcast.xml` (none when there is no release yet, or the latest one has no feed) and runs `scripts/check-release-version.sh`: the tag, the Cargo version and `MARKETING_VERSION` agree, the build number is above the previous one, and the release notes exist.
4. Downloads the Sparkle tools at a pinned version and checksum, and writes the Sparkle key to a file only the runner can read.
5. Imports the certificate into a temporary keychain with a random password. Masks the signing identity, owner name and Team ID before building.
6. Builds and signs the app with `scripts/build-app.sh` and checks that its version and build number are the ones from step 3.
7. Checks that the Sparkle key matches the app's `SUPublicEDKey`. The release stops here when it does not.
8. Notarizes the app and staples the ticket to it.
9. Builds and signs the DMG with `scripts/make-dmg.sh`, notarizes and staples it, and asks Gatekeeper (`spctl`) to assess it.
10. Signs the DMG with the Sparkle key, verifies the signature, writes `appcast.xml` with `scripts/make-appcast.sh` and writes `SHA256SUMS`.
11. Publishes the GitHub release with the three files, or uploads them to the run when not publishing.
12. Always: deletes the temporary keychain and the key files.

## When notarization fails

The workflow prints the submission id and status, then fails. It does not publish Apple's full report or raw error output, which can include signing metadata and paths. Read the detailed report privately with the same API key; the reasons are in its `issues` list (an unsigned binary, a missing hardened runtime or secure timestamp, and the like):

```sh
xcrun notarytool log <submission-id> --key AuthKey.p8 --key-id <key-id> --issuer <issuer-id>
```

A run that failed before step 11 has published nothing. When the cause was outside the repository (a secret, Apple's service), run the workflow again. When the fix is a commit, merge it to `main` and check it with a manual run without `publish`; the tag then has to point at the new commit, so delete it and tag again after the merge.

## While the repository is private

Release assets and the feed (`https://github.com/TemMax/conductor-remote/releases/latest/download/appcast.xml`) are reachable only by people with access to the repository. Installed copies of anyone else find no update; they get updates once the repository is public.
