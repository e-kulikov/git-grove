#!/usr/bin/env bash
# Usage: check-commit-range.sh <revision-range>
# Runs `committed` (rules in committed.toml) on every non-merge commit in the range.
# `committed <range>` also judges merge commits, including the synthetic merge commit
# a pull request is checked out as, so the commits are fed to it one at a time.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <revision-range>" >&2
  exit 2
fi
if ! command -v committed >/dev/null; then
  echo "committed is not installed; run 'mise install'" >&2
  exit 1
fi

status=0
while IFS= read -r sha; do
  git log -1 --format=%B "$sha" | committed --commit-file - 2>&1 | sed "s/^/${sha:0:7}: /" >&2 ||
    status=1
done < <(git rev-list --no-merges "$1")
exit "$status"
