#!/usr/bin/env bash
# Builds and signs <DIR>/Conductor Remote.app: the menu-bar app with the relay inside it.
# Usage: scripts/build-app.sh [--adhoc] [--skip-web] [--out DIR]
set -euo pipefail

start_dir="$PWD"
cd "$(git rev-parse --show-toplevel)"

APP_NAME="Conductor Remote"
RELAY_ID="com.temmax.conductor-remote.relay"
adhoc=0
skip_web=0
out=""

usage() {
	echo 'usage: scripts/build-app.sh [--adhoc] [--skip-web] [--out DIR]' >&2
}

while [ $# -gt 0 ]; do
	case "$1" in
	--adhoc) adhoc=1 ;;
	--skip-web) skip_web=1 ;;
	--out)
		[ $# -ge 2 ] && [ -n "$2" ] || { usage; exit 2; }
		out="$2"
		shift
		;;
	-h | --help) usage; exit 0 ;;
	*) echo "unknown argument: $1" >&2; usage; exit 2 ;;
	esac
	shift
done
# The default is build/ in the repository; a relative --out is taken from where the script was called.
case "$out" in
"") out="build" ;;
/*) ;;
*) out="$start_dir/$out" ;;
esac

if ! command -v xcodegen >/dev/null 2>&1; then
	echo "error: xcodegen is not installed (brew install xcodegen); it generates the app's Xcode project." >&2
	exit 1
fi

if [ "$adhoc" -eq 1 ]; then
	identity="-"
else
	identity="$(security find-identity -v -p codesigning | sed -n 's/^[[:space:]]*[0-9]*) [0-9A-F]* "\(Developer ID Application: .*\)"$/\1/p' | sed -n 1p)"
	if [ -z "$identity" ]; then
		echo "error: no 'Developer ID Application' signing identity found in the keychain." >&2
		echo "Install one, or use --adhoc for a local build." >&2
		exit 1
	fi
fi

if [ "$skip_web" -eq 0 ]; then
	npm --prefix web ci
	npm --prefix web run build
fi
if [ ! -f web/dist/index.html ]; then
	echo "error: web/dist is not built; run without --skip-web." >&2
	exit 1
fi

cargo build --release -p conductor-remote

xcodegen generate --spec app/project.yml --quiet
xcodebuild -project "app/$APP_NAME.xcodeproj" -scheme "$APP_NAME" -configuration Release \
	-destination 'platform=macOS' -derivedDataPath build/dd CODE_SIGNING_ALLOWED=NO build -quiet

built="build/dd/Build/Products/Release/$APP_NAME.app"
[ -d "$built" ] || { echo "error: xcodebuild left no $built" >&2; exit 1; }

app="$out/$APP_NAME.app"
rm -rf "$app"
mkdir -p "$out"
ditto "$built" "$app"
cp target/release/conductor-remote "$app/Contents/MacOS/conductor-remote"

if [ "$adhoc" -eq 0 ]; then
	ed_key="$(/usr/libexec/PlistBuddy -c 'Print :SUPublicEDKey' "$app/Contents/Info.plist" 2>/dev/null || true)"
	case "$ed_key" in
	"" | "<"*">")
		echo "error: SUPublicEDKey in the built Info.plist is empty or a placeholder; set it in app/project.yml." >&2
		exit 1
		;;
	esac
fi

# Credits.html: this project's licence and Sparkle's, as preformatted text.
sparkle_license="build/dd/SourcePackages/checkouts/Sparkle/LICENSE"
if [ ! -f "$sparkle_license" ]; then
	sparkle_license="$(find build/dd/SourcePackages -type f -name LICENSE -path '*[Ss]parkle*' | sort | sed -n 1p)"
fi
if [ -z "$sparkle_license" ] || [ ! -f "$sparkle_license" ]; then
	echo "error: Sparkle's LICENSE was not found under build/dd/SourcePackages." >&2
	exit 1
fi
if ! grep -q '^## License$' README.md; then
	echo "error: README.md has no '## License' section." >&2
	exit 1
fi
escape_html() {
	sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
}
{
	printf '<!DOCTYPE html>\n<html>\n<head>\n<meta charset="utf-8">\n<title>%s</title>\n</head>\n<body>\n<pre>\n' "$APP_NAME"
	sed -n '/^## License$/,$p' README.md | escape_html
	printf '</pre>\n<pre>\nSparkle\n\n'
	escape_html <"$sparkle_license"
	printf '</pre>\n</body>\n</html>\n'
} >"$app/Contents/Resources/Credits.html"

# Sign inside out, never with --deep.
sign() {
	if [ "$adhoc" -eq 1 ]; then
		codesign --force --options runtime --sign "$identity" "$@"
	else
		codesign --force --options runtime --sign "$identity" --timestamp "$@"
	fi
}

sparkle="$app/Contents/Frameworks/Sparkle.framework"
[ -d "$sparkle/Versions/B" ] || { echo "error: $sparkle/Versions/B is missing from the built app." >&2; exit 1; }
for xpc in "$sparkle/Versions/B/XPCServices/"*.xpc; do
	[ -e "$xpc" ] || { echo "error: Sparkle.framework holds no XPC services." >&2; exit 1; }
	if [ "$(basename "$xpc")" = "Downloader.xpc" ]; then
		sign --preserve-metadata=entitlements "$xpc"
	else
		sign "$xpc"
	fi
done
sign "$sparkle/Versions/B/Autoupdate"
sign "$sparkle/Versions/B/Updater.app"
sign "$sparkle"
sign --identifier "$RELAY_ID" "$app/Contents/MacOS/conductor-remote"
sign --entitlements app/App/ConductorRemote.entitlements "$app"

codesign --verify --deep --strict --verbose=2 "$app"
echo "$app"
codesign -dv "$app"
