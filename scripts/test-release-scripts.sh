#!/usr/bin/env bash
# Tests for the release scripts. Everything runs in a temporary directory; no real keys are used.
set -uo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
scripts="$repo/scripts"
fixtures="$scripts/fixtures/release"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/release-scripts-test.XXXXXX")
mount_dir=""
cleanup() {
	if [ -n "$mount_dir" ]; then hdiutil detach -quiet "$mount_dir" >/dev/null 2>&1 || true; fi
	rm -rf "$tmp"
}
trap cleanup EXIT

failed=0
check() {
	local name=$1
	shift
	local out
	if out=$("$@" 2>&1); then
		echo "ok    $name"
	else
		echo "FAIL  $name"
		printf '%s\n' "$out" | sed 's/^/      /'
		failed=1
	fi
}

# --- helpers (each returns non-zero on the first failed step) ---

fails() { ! "$@"; }

exits_with() {
	local want=$1 got=0
	shift
	"$@" >/dev/null 2>&1 || got=$?
	[ "$got" -eq "$want" ]
}

contains() { grep -qF -- "$2" "$1"; }

notes_html() { bash "$scripts/notes-to-html.sh" "$fixtures/notes.md" >"$tmp/notes.html"; }
notes_has() { notes_html && grep -qxF -- "$1" "$tmp/notes.html"; }
notes_matches_fixture() { notes_html && diff "$tmp/notes.html" "$fixtures/notes.expected.html"; }
notes_has_one_list() { notes_html && [ "$(grep -c '^<ul>$' "$tmp/notes.html")" -eq 1 ] && [ "$(grep -c '^<li>' "$tmp/notes.html")" -eq 3 ]; }
notes_has_no_raw_markup() { notes_html && ! grep -q '<script' "$tmp/notes.html"; }

sig='sparkle:edSignature="dGVzdC1zaWduYXR1cmU=" length="12345"'
url='https://example.com/Conductor-Remote-0.1.0.dmg'
appcast() { bash "$scripts/make-appcast.sh" "${1:-0.1.0}" "${2:-7}" "${3:-$url}" "${4:-$sig}" "$fixtures/notes.md"; }
appcast_to_file() { appcast >"$tmp/appcast.xml"; }
appcast_well_formed() { appcast_to_file && xmllint --noout "$tmp/appcast.xml"; }
appcast_field() { appcast_to_file && contains "$tmp/appcast.xml" "$1"; }
appcast_pubdate_rfc822() { appcast_to_file && grep -Eq '<pubDate>[A-Z][a-z]{2}, [0-9]{2} [A-Z][a-z]{2} [0-9]{4} [0-9]{2}:[0-9]{2}:[0-9]{2} \+0000</pubDate>' "$tmp/appcast.xml"; }
appcast_carries_notes() { appcast_to_file && contains "$tmp/appcast.xml" '<li>A <strong>bold</strong> word in the second bullet</li>'; }

make_app() {
	local app=$tmp/Fake.app
	mkdir -p "$app/Contents/MacOS" || return 1
	cat >"$app/Contents/Info.plist" <<'PLIST' || return 1
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>CFBundleExecutable</key><string>Fake</string></dict></plist>
PLIST
	printf '#!/bin/sh\nexit 0\n' >"$app/Contents/MacOS/Fake" || return 1
	chmod +x "$app/Contents/MacOS/Fake"
}
dmg_path=""
build_dmg() {
	make_app || return 1
	dmg_path=$(bash "$scripts/make-dmg.sh" "$tmp/Fake.app" 0.1.0 "$tmp/out/nested") || return 1
	[ "$dmg_path" = "$tmp/out/nested/Conductor-Remote-0.1.0.dmg" ]
}
dmg_exists() { build_dmg && [ -s "$dmg_path" ]; }
dmg_readable() { build_dmg && hdiutil imageinfo "$dmg_path" >/dev/null; }
dmg_holds_app_and_link() {
	build_dmg || return 1
	local mnt=$tmp/mnt
	mkdir -p "$mnt" || return 1
	hdiutil attach -readonly -nobrowse -quiet -mountpoint "$mnt" "$dmg_path" || return 1
	mount_dir=$mnt
	local ok=0
	{ [ -x "$mnt/Fake.app/Contents/MacOS/Fake" ] && [ -L "$mnt/Applications" ] && [ "$(readlink "$mnt/Applications")" = /Applications ]; } || ok=1
	hdiutil detach -quiet "$mnt" || ok=1
	mount_dir=""
	return $ok
}
dmg_rejects_bad_version() { make_app && fails bash "$scripts/make-dmg.sh" "$tmp/Fake.app" 1.0 "$tmp/out"; }

root=$tmp/root
make_root() {
	rm -rf "$root"
	mkdir -p "$root/app" "$root/docs/release-notes" || return 1
	cp "$fixtures/Cargo.toml" "$root/Cargo.toml" || return 1
	cp "$fixtures/project.yml" "$root/app/project.yml"
}
version_check() { make_root && bash "$scripts/check-release-version.sh" "$@" --root "$root"; }
version_prints_fields() {
	local out
	out=$(version_check v0.1.0 --skip-notes) || return 1
	[ "$out" = "$(printf 'version=0.1.0\nbuild=1')" ]
}
version_marketing_disagrees() {
	make_root || return 1
	sed -i.bak 's/MARKETING_VERSION: 0.1.0/MARKETING_VERSION: 0.2.0/' "$root/app/project.yml" || return 1
	fails bash "$scripts/check-release-version.sh" v0.1.0 --skip-notes --root "$root"
}
version_build_not_above() {
	make_root || return 1
	local build
	build=$(bash "$scripts/check-release-version.sh" v0.1.0 --skip-notes --root "$root" | sed -n 's/^build=//p') || return 1
	bash "$scripts/check-release-version.sh" v0.1.0 --skip-notes --previous-build $((build - 1)) --root "$root" >/dev/null || return 1
	fails bash "$scripts/check-release-version.sh" v0.1.0 --skip-notes --previous-build "$build" --root "$root"
}
version_notes_missing() { make_root && fails bash "$scripts/check-release-version.sh" v0.1.0 --root "$root"; }
version_notes_empty() { make_root && : >"$root/docs/release-notes/v0.1.0.md" && fails bash "$scripts/check-release-version.sh" v0.1.0 --root "$root"; }
version_notes_present() { make_root && cp "$fixtures/notes.md" "$root/docs/release-notes/v0.1.0.md" && bash "$scripts/check-release-version.sh" v0.1.0 --root "$root" >/dev/null; }
version_debug_override_ignored() { version_check v0.1.0 --skip-notes | grep -qx 'build=1'; }

keygen_swift=$tmp/keygen.swift
cat >"$keygen_swift" <<'SWIFT'
import CryptoKit
import Foundation
let dir = CommandLine.arguments[1]
let key = Curve25519.Signing.PrivateKey()
let other = Curve25519.Signing.PrivateKey()
let publicKey = key.publicKey.rawRepresentation
func write(_ text: String, _ name: String) { try! text.write(toFile: dir + "/" + name, atomically: true, encoding: .utf8) }
write("  " + key.rawRepresentation.base64EncodedString() + "\n\n", "seed.key")
write(publicKey.base64EncodedString(), "public.txt")
write(other.publicKey.rawRepresentation.base64EncodedString(), "other-public.txt")
var legacy = Data((0..<64).map { _ in UInt8.random(in: 0...255) })
legacy.append(publicKey)
write(legacy.base64EncodedString(), "legacy.key")
write("this is not base64 !!", "garbage.key")
write(Data(repeating: 7, count: 16).base64EncodedString(), "short.key")
SWIFT
keys=$tmp/keys
keygen() { [ -s "$keys/public.txt" ] || { mkdir -p "$keys" && swift "$keygen_swift" "$keys"; }; }
sparkle_key() { keygen && swift "$scripts/check-sparkle-key.swift" "$keys/$1" "$(cat "$keys/$2")"; }
key_exits() { keygen && exits_with "$1" swift "$scripts/check-sparkle-key.swift" "$keys/$2" "$(cat "$keys/$3")"; }
key_never_printed() { keygen && ! swift "$scripts/check-sparkle-key.swift" "$keys/seed.key" "$(cat "$keys/other-public.txt")" 2>&1 | grep -qF "$(tr -d ' \n' <"$keys/seed.key")"; }

# --- notes-to-html ---
check "notes-to-html: headings" notes_has '<h2>Added</h2>'
check "notes-to-html: consecutive bullets form one list" notes_has_one_list
check "notes-to-html: paragraph lines join with a space" notes_has '<p>The first line of a paragraph continues on a second line.</p>'
check "notes-to-html: code, bold and link" notes_has '<li>A <a href="https://example.com/page?a=1&amp;b=2">link</a> in the third bullet</li>'
check "notes-to-html: < is escaped, never markup" notes_has_no_raw_markup
check "notes-to-html: whole fixture matches the expected fragment" notes_matches_fixture
check "notes-to-html: refuses a missing file" exits_with 2 bash "$scripts/notes-to-html.sh" "$tmp/none.md"

# --- make-appcast ---
check "make-appcast: feed is well-formed XML" appcast_well_formed
check "make-appcast: version, build and minimum system version" appcast_field '<sparkle:shortVersionString>0.1.0</sparkle:shortVersionString>'
check "make-appcast: build number" appcast_field '<sparkle:version>7</sparkle:version>'
check "make-appcast: minimum system version" appcast_field '<sparkle:minimumSystemVersion>14.0</sparkle:minimumSystemVersion>'
check "make-appcast: URL and signature attributes verbatim" appcast_field "<enclosure url=\"$url\" $sig type=\"application/octet-stream\" />"
check "make-appcast: pubDate is RFC 822 in UTC" appcast_pubdate_rfc822
check "make-appcast: description carries the notes" appcast_carries_notes
check "make-appcast: refuses malformed sig-attrs" fails appcast 0.1.0 7 "$url" 'sparkle:edSignature="abc"'
check "make-appcast: refuses a signature without a length" fails appcast 0.1.0 7 "$url" 'sparkle:edSignature="abc=" length="x"'
check "make-appcast: refuses a malformed version" fails appcast 1.0 7 "$url" "$sig"
check "make-appcast: refuses a malformed build" fails appcast 0.1.0 0 "$url" "$sig"

# --- make-dmg ---
check "make-dmg: the DMG exists" dmg_exists
check "make-dmg: hdiutil reads it" dmg_readable
check "make-dmg: holds the app and the Applications link" dmg_holds_app_and_link
check "make-dmg: refuses a malformed version" dmg_rejects_bad_version

# --- check-release-version ---
check "check-release-version: current versions pass and print version and build" version_prints_fields
check "check-release-version: the Debug build override is ignored" version_debug_override_ignored
check "check-release-version: a wrong tag fails" fails version_check v0.2.0 --skip-notes
check "check-release-version: a malformed tag fails" fails version_check 0.1.0 --skip-notes
check "check-release-version: a disagreeing MARKETING_VERSION fails" version_marketing_disagrees
check "check-release-version: a build not above --previous-build fails" version_build_not_above
check "check-release-version: missing notes fail" version_notes_missing
check "check-release-version: empty notes fail" version_notes_empty
check "check-release-version: present notes pass" version_notes_present

# --- check-sparkle-key ---
check "check-sparkle-key: the matching public key passes (seed with whitespace)" sparkle_key seed.key public.txt
check "check-sparkle-key: another public key fails with 1" key_exits 1 seed.key other-public.txt
check "check-sparkle-key: the 96-byte format passes" sparkle_key legacy.key public.txt
check "check-sparkle-key: the 96-byte format with another key fails with 1" key_exits 1 legacy.key other-public.txt
check "check-sparkle-key: garbage input exits 2" key_exits 2 garbage.key public.txt
check "check-sparkle-key: a key of another length exits 2" key_exits 2 short.key public.txt
check "check-sparkle-key: a missing file exits 2" key_exits 2 absent.key public.txt
check "check-sparkle-key: never prints the private key" key_never_printed

if [ "$failed" -ne 0 ]; then
	echo "release script tests FAILED" >&2
	exit 1
fi
echo "release script tests passed"
