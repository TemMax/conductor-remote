#!/usr/bin/env bash
# Runs the kit's tests and builds the app target unsigned.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

if ! command -v xcodegen >/dev/null 2>&1; then
	echo "error: xcodegen is not installed (brew install xcodegen); it generates the app's Xcode project." >&2
	exit 1
fi

swift test --package-path app
xcodegen generate --spec app/project.yml --quiet
xcodebuild -project "app/Conductor Remote.xcodeproj" -scheme "Conductor Remote" -configuration Debug \
	-destination 'platform=macOS' -derivedDataPath build/check-dd CODE_SIGNING_ALLOWED=NO build -quiet
