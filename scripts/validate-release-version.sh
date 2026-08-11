#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <vX.Y.Z> <Cargo.toml>" >&2
  exit 64
fi

tag=$1
manifest=$2

if [[ ! $tag =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "release tag must be strict vX.Y.Z: $tag" >&2
  exit 64
fi

if [[ ! -f $manifest ]]; then
  echo "Cargo manifest not found: $manifest" >&2
  exit 1
fi

manifest_version=$(
  awk '
    /^\[package\][[:space:]]*$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^[[:space:]]*version[[:space:]]*=/ {
      line = $0
      sub(/^[^=]*=[[:space:]]*"/, "", line)
      sub(/"[[:space:]]*$/, "", line)
      print line
      exit
    }
  ' "$manifest"
)

if [[ -z $manifest_version ]]; then
  echo "package version not found in $manifest" >&2
  exit 1
fi

tag_version=${tag#v}
if [[ $tag_version != "$manifest_version" ]]; then
  echo "tag version $tag_version does not match Cargo package version $manifest_version" >&2
  exit 64
fi

printf '%s\n' "$tag_version"
