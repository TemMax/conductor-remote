#!/usr/bin/env bash
# Fails when a file other than README.md names the project this one was inspired by.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
pattern='h[y]ldmo|adl[u]na|E[i]vind'
if git grep --untracked -IinE "$pattern" -- . ':!README.md'; then
	echo "identity check failed: the names above may appear in README.md only" >&2
	exit 1
fi
echo "identity check ok"
