#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <X.Y.Z> <git-grove-binary> <destination>" >&2
  exit 64
fi

version=$1
binary=$2
destination=$3

if [[ ! $version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "release version must be strict X.Y.Z: $version" >&2
  exit 64
fi
if [[ ! -f $binary || ! -x $binary ]]; then
  echo "release binary must be an executable regular file: $binary" >&2
  exit 1
fi

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
source_date_epoch=${SOURCE_DATE_EPOCH:-0}
if [[ ! $source_date_epoch =~ ^[0-9]+$ ]]; then
  echo "SOURCE_DATE_EPOCH must be a non-negative integer" >&2
  exit 64
fi

stage_name="git-grove_${version}_linux_x86_64"
archive_name="$stage_name.tar.gz"
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/git-grove-release.XXXXXX")
trap 'rm -rf -- "$tmp_dir"' EXIT
stage="$tmp_dir/$stage_name"

install -d -m 0755 "$stage/completions" "$stage/man"
install -m 0755 "$binary" "$stage/git-grove"
install -m 0644 "$repo_root/LICENSE" "$stage/LICENSE"
install -m 0644 "$repo_root/README.md" "$stage/README.md"
install -m 0644 "$repo_root/man/git-grove.1" "$stage/man/git-grove.1"

"$binary" completion bash >"$stage/completions/git-grove.bash"
"$binary" completion zsh >"$stage/completions/_git-grove"
"$binary" completion fish >"$stage/completions/git-grove.fish"
for completion in "$stage"/completions/*; do
  if [[ ! -s $completion ]]; then
    echo "generated completion is empty: $completion" >&2
    exit 1
  fi
  chmod 0644 "$completion"
done

LC_ALL=C tar \
  --sort=name \
  --format=gnu \
  --owner=0 \
  --group=0 \
  --numeric-owner \
  --mtime="@$source_date_epoch" \
  -C "$tmp_dir" \
  -cf - "$stage_name" | gzip -n >"$tmp_dir/$archive_name"

install -d -m 0755 "$destination"
install -m 0644 "$tmp_dir/$archive_name" "$destination/$archive_name"
(
  cd -- "$destination"
  sha256sum "$archive_name" >SHA256SUMS.tmp
  mv -f -- SHA256SUMS.tmp SHA256SUMS
)
