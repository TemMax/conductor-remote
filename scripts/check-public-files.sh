#!/usr/bin/env bash
# Reject private agent artifacts and local credentials without logging their names or contents.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

git ls-files -z | while IFS= read -r -d '' file; do
	case "$file" in
	docs/specs/* | docs/plans/* | docs/superpowers/* | .local/* | .agents/* | .codex/* | .claude/* | .superpowers/* | .temmax/* | .orchestration/* | .worktrees/* | worktrees/* | agents.local.md | AGENTS.local.md | AGENTS.override.md | */agents.local.md | */AGENTS.local.md | */AGENTS.override.md)
		echo 'error: private agent instructions or working artifacts are tracked; inspect git ls-files locally' >&2
		exit 1
		;;
	.env.example | .env.sample | */.env.example | */.env.sample) ;;
	.env | .env.* | */.env | */.env.* | *.p12 | *.p8 | *.pem | *.key | *.token | *.db | *.db-* | *.sqlite | *.sqlite-* | *.sqlite3 | *.sqlite3-* | *.log)
		echo 'error: local credentials or runtime data are tracked; inspect git ls-files locally' >&2
		exit 1
		;;
	esac
done

echo 'public file policy ok'
