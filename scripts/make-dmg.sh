#!/usr/bin/env bash
# Builds <out-dir>/Conductor-Remote-<version>.dmg holding the app and an Applications link.
set -euo pipefail

usage() {
	echo "usage: make-dmg.sh <app> <version> <out-dir> [--identity <name>]" >&2
	exit 2
}

[ $# -ge 3 ] || usage
app=$1
version=$2
out_dir=$3
shift 3
identity=""
if [ $# -gt 0 ]; then
	[ "$1" = "--identity" ] && [ $# -eq 2 ] && [ -n "$2" ] || usage
	identity=$2
fi

if [ ! -d "$app" ]; then
	echo "error: $app is not a directory" >&2
	exit 1
fi
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
	echo "error: the version '$version' is not of the form x.y.z" >&2
	exit 1
fi

mkdir -p "$out_dir"
dmg="$out_dir/Conductor-Remote-$version.dmg"

staging=$(mktemp -d "${TMPDIR:-/tmp}/conductor-remote-dmg.XXXXXX")
trap 'rm -rf "$staging"' EXIT

ditto "$app" "$staging/$(basename "$app")"
ln -s /Applications "$staging/Applications"

hdiutil create -quiet -volname "Conductor Remote" -srcfolder "$staging" -format UDZO -ov "$dmg"
if [ -n "$identity" ]; then
	codesign --force --timestamp --sign "$identity" "$dmg"
fi
echo "$dmg"
