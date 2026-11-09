#!/usr/bin/env bash
# Points git at the repository's own hooks in .githooks/ (today: a
# pre-commit hook that runs scripts/clippy.sh and blocks new warnings).
# Run it once per clone; worktrees share the clone's config.
set -euo pipefail
cd "$(dirname "$0")/.."
git config core.hooksPath .githooks
chmod +x .githooks/* scripts/clippy.sh
echo "install-hooks: core.hooksPath = $(git config --get core.hooksPath)"
