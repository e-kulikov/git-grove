#!/usr/bin/env bash
# Tests for the release and commit-hygiene scripts; needs bash, git, tar, gzip and committed.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
temp=$(mktemp -d)
trap 'rm -rf -- "$temp"' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# --- validate-release-version.sh
printf '[package]\nname = "x"\nversion = "1.2.3"\n' >"$temp/Cargo.toml"
[[ $("$repo/scripts/validate-release-version.sh" v1.2.3 "$temp/Cargo.toml") == 1.2.3 ]] || fail "version output"
"$repo/scripts/validate-release-version.sh" v1.2.4 "$temp/Cargo.toml" 2>/dev/null && fail "mismatched tag accepted"
"$repo/scripts/validate-release-version.sh" 1.2.3 "$temp/Cargo.toml" 2>/dev/null && fail "tag without v accepted"
"$repo/scripts/validate-release-version.sh" v1.2.3-rc1 "$temp/Cargo.toml" 2>/dev/null && fail "pre-release tag accepted"

# --- package-release.sh with a stand-in binary
cat >"$temp/git-grove" <<'SCRIPT'
#!/bin/sh
# Stand-in for `git-grove completion <shell>`.
[ "$1" = completion ] && printf '# %s completion\n' "$2"
SCRIPT
chmod 0755 "$temp/git-grove"
export SOURCE_DATE_EPOCH=1700000000
first=$("$repo/scripts/package-release.sh" 1.2.3 x86_64-unknown-linux-musl "$temp/git-grove" "$temp/out1")
second=$("$repo/scripts/package-release.sh" 1.2.3 x86_64-unknown-linux-musl "$temp/git-grove" "$temp/out2")
[[ $(basename "$first") == git-grove_1.2.3_linux_x86_64.tar.gz ]] || fail "archive name: $first"
cmp "$first" "$second" || fail "archives are not reproducible"
[[ ! -e $temp/out1/SHA256SUMS ]] || fail "the packaging script must not write checksums"

stage=git-grove_1.2.3_linux_x86_64
listing=$(tar -tzf "$first")
for expected in \
  "$stage/git-grove" \
  "$stage/LICENSE" \
  "$stage/README.md" \
  "$stage/man/git-grove.1" \
  "$stage/completions/git-grove.bash" \
  "$stage/completions/_git-grove" \
  "$stage/completions/git-grove.fish"; do
  grep -Fxq "$expected" <<<"$listing" || fail "missing $expected"
done
verbose=$(tar -tvzf "$first")
grep -q "rwxr-xr-x 0/0 .* $stage/git-grove\$" <<<"$verbose" || fail "binary mode or owner"
grep -q '2023-11-14' <<<"$verbose" || fail "timestamps are not pinned"

"$repo/scripts/package-release.sh" 1.2.3 riscv64-unknown-linux-musl "$temp/git-grove" "$temp/out3" 2>/dev/null &&
  fail "unsupported target accepted"
aarch=$("$repo/scripts/package-release.sh" 1.2.3 aarch64-unknown-linux-musl "$temp/git-grove" "$temp/out4")
[[ $(basename "$aarch") == git-grove_1.2.3_linux_aarch64.tar.gz ]] || fail "aarch64 name"

# --- check-commit-range.sh and committed.toml, in a scratch repository
git init -q "$temp/repo"
git -C "$temp/repo" config user.email t@example.invalid
git -C "$temp/repo" config user.name T
cp "$repo/committed.toml" "$temp/repo/committed.toml"
commit() { git -C "$temp/repo" commit -q --allow-empty -m "$1"; }
check() { (cd "$temp/repo" && "$repo/scripts/check-commit-range.sh" "$1"); }
commit "chore: start"
commit "feat(publish)!: breaking change"
commit "ci: lowercase subjects longer than fifty characters are this repository's style"
commit "revert: undo a change"
check HEAD || fail "valid subjects rejected"
git -C "$temp/repo" checkout -q -b side
commit "fix: on a side branch"
git -C "$temp/repo" checkout -q -
git -C "$temp/repo" merge -q --no-ff -m "Merge branch 'side'" side
check HEAD || fail "merge commit was judged"
commit "Fix the thing"
check HEAD 2>/dev/null && fail "unconventional subject accepted"
git -C "$temp/repo" reset -q --hard HEAD~1
commit "wip: half done"
check HEAD 2>/dev/null && fail "unknown type accepted"
git -C "$temp/repo" reset -q --hard HEAD~1

# --- install-git-hooks.sh and the commit-msg hook
hooks="$temp/repo/.git/hooks"
(cd "$temp/repo" && "$repo/scripts/install-git-hooks.sh" >/dev/null) || fail "hook install"
cmp "$repo/scripts/git-hooks/commit-msg" "$hooks/commit-msg" || fail "installed hook differs"
(cd "$temp/repo" && "$repo/scripts/install-git-hooks.sh" >/dev/null) || fail "hook reinstall is not idempotent"
git -C "$temp/repo" commit -q --allow-empty -m "feat: accepted by the hook" || fail "hook rejected a good message"
git -C "$temp/repo" commit -q --allow-empty -m "Not conventional" 2>/dev/null && fail "hook accepted a bad message"
printf '#!/bin/sh\nexit 0\n' >"$hooks/commit-msg"
(cd "$temp/repo" && "$repo/scripts/install-git-hooks.sh" 2>/dev/null) && fail "foreign hook overwritten"
grep -q 'exit 0' "$hooks/commit-msg" || fail "foreign hook was modified"

echo "release script tests passed"
