#!/usr/bin/env bash
# Checks that a release tag agrees with the Cargo workspace version, the app's marketing version
# and build number, and that the release notes exist. Prints version=<x.y.z> and build=<n>.
set -euo pipefail

usage() {
	echo "usage: check-release-version.sh <tag> [--previous-build N] [--skip-notes] [--root DIR]" >&2
	exit 2
}

tag=""
previous_build=""
skip_notes=0
root=""
while [ $# -gt 0 ]; do
	case "$1" in
	--previous-build)
		[ $# -ge 2 ] || usage
		previous_build=$2
		shift 2
		;;
	--skip-notes)
		skip_notes=1
		shift
		;;
	--root)
		[ $# -ge 2 ] || usage
		root=$2
		shift 2
		;;
	-*) usage ;;
	*)
		[ -z "$tag" ] || usage
		tag=$1
		shift
		;;
	esac
done
[ -n "$tag" ] || usage
if [ -n "$previous_build" ] && ! [[ "$previous_build" =~ ^[0-9]+$ ]]; then
	echo "error: --previous-build must be a non-negative integer, got '$previous_build'" >&2
	exit 2
fi
if [ -z "$root" ]; then
	root=$(git rev-parse --show-toplevel)
fi

errors=()
fail() { errors+=("$1"); }

# The `version` of the [workspace.package] table.
cargo_version=$(awk '
	/^\[/ { in_table = ($0 ~ /^\[workspace\.package\][[:space:]]*$/); next }
	in_table && /^[[:space:]]*version[[:space:]]*=/ {
		v = $0
		sub(/^[^=]*=[[:space:]]*/, "", v)
		sub(/[[:space:]]*(#.*)?$/, "", v)
		gsub(/"/, "", v)
		print v
		exit
	}
' "$root/Cargo.toml" 2>/dev/null || true)

# A key of the app target's base settings (not of a per-configuration override under `configs:`).
project_setting() {
	awk -v key="$1" '
		function indent(s) { match(s, /[^ ]/); return RSTART }
		/^[[:space:]]*base:[[:space:]]*$/ { base_indent = indent($0); in_base = 1; next }
		in_base {
			if ($0 ~ /^[[:space:]]*$/) next
			if (indent($0) <= base_indent) { in_base = 0; next }
			if ($0 ~ "^[[:space:]]*" key ":") {
				v = $0
				sub(/^[^:]*:[[:space:]]*/, "", v)
				sub(/[[:space:]]*(#.*)?$/, "", v)
				gsub(/"/, "", v)
				print v
				exit
			}
		}
	' "$root/app/project.yml" 2>/dev/null || true
}
marketing_version=$(project_setting MARKETING_VERSION)
build=$(project_setting CURRENT_PROJECT_VERSION)

if ! [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
	fail "the tag '$tag' is not of the form v<major>.<minor>.<patch>"
fi
if [ -z "$cargo_version" ]; then
	fail "no version found in the [workspace.package] table of $root/Cargo.toml"
elif [ "$tag" != "v$cargo_version" ]; then
	fail "the tag '$tag' disagrees with the Cargo workspace version $cargo_version"
fi
if [ -z "$marketing_version" ]; then
	fail "no MARKETING_VERSION found in the base settings of $root/app/project.yml"
elif [ "$tag" != "v$marketing_version" ]; then
	fail "the tag '$tag' disagrees with MARKETING_VERSION $marketing_version"
fi
if ! [[ "$build" =~ ^[1-9][0-9]*$ ]]; then
	fail "CURRENT_PROJECT_VERSION '$build' is not a positive integer"
elif [ -n "$previous_build" ] && [ "$build" -le "$previous_build" ]; then
	fail "CURRENT_PROJECT_VERSION $build is not greater than the previous build $previous_build"
fi
if [ "$skip_notes" -eq 0 ]; then
	notes="$root/docs/release-notes/$tag.md"
	if [ ! -s "$notes" ]; then
		fail "the release notes docs/release-notes/$tag.md are missing or empty"
	fi
fi

if [ ${#errors[@]} -gt 0 ]; then
	for e in "${errors[@]}"; do
		echo "error: $e" >&2
	done
	exit 1
fi
echo "version=${tag#v}"
echo "build=$build"
