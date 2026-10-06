#!/usr/bin/env bash
# Usage: check-conventional-commits.sh <revision-range>
# Fails when a non-merge commit subject is not a Conventional Commit.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <revision-range>" >&2
  exit 2
fi

pattern='^(feat|fix|docs|refactor|test|build|ci|chore|perf|style|revert)(\([a-z0-9._/-]+\))?!?: .+'
status=0
while IFS= read -r subject; do
  if [[ ! $subject =~ $pattern ]]; then
    echo "not a Conventional Commit subject: $subject" >&2
    status=1
  fi
done < <(git log --no-merges --format=%s "$1")
exit "$status"
