#!/usr/bin/env bash
# Usage: install-git-hooks.sh
# Installs scripts/git-hooks/commit-msg into the repository's shared hooks directory.
# `git rev-parse --git-common-dir` resolves to .git in a normal clone and to the shared
# bare repository in a grove, so one install covers every worktree. Safe to re-run;
# refuses to replace a commit-msg hook this script did not install.
set -euo pipefail

if [[ $# -ne 0 ]]; then
  echo "usage: $0" >&2
  exit 2
fi

source_hook=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/git-hooks/commit-msg
common_dir=$(git rev-parse --path-format=absolute --git-common-dir)
hooks_dir=${common_dir}/hooks
target=$hooks_dir/commit-msg

mkdir -p "$hooks_dir"
if [[ -e $target || -L $target ]]; then
  if cmp -s "$source_hook" "$target"; then
    echo "commit-msg hook already installed: $target"
    exit 0
  fi
  if ! grep -Fq 'Installed by scripts/install-git-hooks.sh' "$target" 2>/dev/null; then
    echo "refusing to overwrite a different commit-msg hook: $target" >&2
    echo "move it aside or merge it into scripts/git-hooks/commit-msg, then re-run" >&2
    exit 1
  fi
  install -m 0755 "$source_hook" "$target"
  echo "commit-msg hook updated: $target"
  exit 0
fi
install -m 0755 "$source_hook" "$target"
echo "commit-msg hook installed: $target"
if ! command -v committed >/dev/null; then
  echo "note: committed is not on PATH yet; run 'mise install'" >&2
fi
