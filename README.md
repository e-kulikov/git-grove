# git-grove

`git-grove` manages a repository as a bare clone surrounded by Git worktrees.
Version 0.3 has a small, explicit lifecycle of six commands: create or clone a
grove, add worktrees, inspect their state, fetch and fast-forward eligible
worktrees, and publish an unpublished grove to a remote.

## Requirements

- 64-bit Linux (`x86_64`)
- Git 2.47 or newer

The release binary is statically linked and does not require a system Rust
installation.

## Install

With [mise](https://mise.jdx.dev/dev-tools/backends/github.html) and GitHub as
the backend:

```sh
mise use -g 'github:e-kulikov/git-grove@0.3.0'
```

For declarative mise configuration:

```toml
[tools]
"github:e-kulikov/git-grove" = { version = "0.3.0", asset_pattern = "git-grove_{{ version }}_linux_x86_64.tar.gz", strip_components = 1 }
```

The GitHub backend can install this only after the `v0.3.0` release and its
assets have been published. The release workflow produces an attestation and
`SHA256SUMS`; mise can lock the published checksum with `mise lock`.

For a direct installation, download
`git-grove_0.3.0_linux_x86_64.tar.gz` and `SHA256SUMS` from the GitHub Release,
verify the archive, then install its binary:

```sh
sha256sum --check SHA256SUMS
tar -xzf git-grove_0.3.0_linux_x86_64.tar.gz
install -m 0755 git-grove_0.3.0_linux_x86_64/git-grove ~/.local/bin/git-grove
```

The archive also contains the man page and generated Bash, Zsh, and Fish
completions.

## Quick start

`git grove` works because Git finds the `git-grove` executable on `PATH`.
Using `git-grove` directly is equivalent.

```sh
# Clone and create the first worktree.
git grove clone https://github.com/example/project.git project
cd project

# Add an existing local or uniquely matching remote branch.
git grove add feature/login

# Create a new branch from a revision.
git grove add experiment --start main

# Inspect every worktree.
git grove list

# Fetch every required remote and fast-forward eligible worktrees.
git grove sync

# Give an unpublished grove a remote and push it.
git grove publish https://github.com/example/project.git
```

The aliases are `plant` for `clone`, `seed` for `init`, `sprout` for `add`,
`survey` for `list`, `tend` for `sync`, and `propagate` for `publish`. Inside a grove, invoking
`git grove` without a command runs `list`. A repository locator as the first
argument selects `clone`.

`add` always names its branch explicitly:

```text
git grove add <branch> [dir]
git grove add --detach <revision> [dir]
```

`--start <revision>` is accepted only when creating a branch that exists
neither locally nor on a remote. A detached worktree defaults to a directory
named `detached-<short-oid>`.

## Layout

```text
project/
├── .bare/       bare Git repository and shared administration
├── .git         pointer containing: gitdir: ./.bare
├── AGENTS.md    generated repository facts and 0.3 command guide
├── CLAUDE.md    relative link to AGENTS.md
├── main/        worktree
└── feature/     another worktree
```

Grove metadata is stored in the real Git configuration at `.bare/config`, not
in a separate metadata file. Version 0.3 uses `grove.version`,
`grove.defaultBranch`, `grove.remote`, `grove.publishState`,
`grove.publishRemote`, and `grove.publishUrl`.

All managed worktree paths must remain strictly below the grove root. Existing
files, symlinks, nonempty destinations, ambiguous remote branches, and
unrecognized repository state are preserved for a human decision.

## Sync

`git grove sync` (alias `tend`) fetches every remote a registered worktree's
branch is configured to track, then fast-forwards each eligible worktree:

```sh
git grove sync
```

Sync is explicit and narrow, by design:

- It fetches only the remotes configured as the upstream of a registered
  branch worktree, one atomic fetch per remote — never `git fetch --all`.
  If any required fetch fails, sync exits with status `1` and updates no
  worktree.
- It updates only a worktree that is clean and behind: no local commits
  ahead of its upstream, no uncommitted changes, and no lock or in-progress
  operation. Every other worktree, and every candidate whose safety-relevant
  identity changed since it was last inspected, is reported and left alone.
- Updates run one worktree at a time, in a stable order sorted by worktree
  path, so behavior does not depend on process scheduling.
- The only update it ever runs is
  `git merge --ff-only --no-edit --no-autostash --no-overwrite-ignore
  <upstream-oid>`, targeting the exact upstream commit re-inspected
  immediately beforehand, not the symbolic `@{upstream}` revision (which
  Git would resolve from live branch configuration at merge time, not from
  the commit sync already validated). Sync never rebases, pushes, invokes
  git-town, or resolves a conflict; a refused or blocked merge is reported
  and left for a human decision, and later worktrees are still processed.
- Any worktree that remains behind, blocked, or otherwise unresolved after
  a sync run causes exit status `2`.

## Publish

`git grove publish <url>` (alias `propagate`) gives an unpublished grove a
remote and pushes it:

```sh
git grove publish https://github.com/example/project.git
git grove publish --remote upstream https://example.invalid/project.git
git grove publish --all-branches https://github.com/example/project.git
```

`--remote <name>` names the remote to create and defaults to `origin`.
`--all-branches` publishes every local branch instead of the default branch
alone.

Publishing is a transaction with three states — `unpublished`, `publishing`,
and `published` — recorded in `grove.publishState`. Its receipt,
`grove.publishRemote` and `grove.publishUrl`, is written **before the first
step that mutates the remote or the local remote configuration**, and never
rolled back. The read-only inspection of the target therefore runs first, and
the user-visible consequence is the point of the ordering: a grove that is
refused while the target is being inspected is left untouched, so a mistyped
URL costs nothing and can simply be republished to a corrected URL.

Publish is explicit and narrow, by design:

- It inspects the target before configuring anything: `git ls-remote --symref`
  reports what the target has, and a branch already there is compared through
  a probe ref this transaction owns, under `refs/grove/publish-probe/`, which
  is deleted after the decision.
- It **never force-pushes**, never rewrites history, never merges a
  host-created commit, never deletes a remote ref, never creates a repository
  on a hosting provider, and never calls a provider API.
- A target whose default branch has diverged from this grove's, or whose
  history is unrelated to it, is refused with exit status `2` and nothing is
  pushed.
- A target whose `HEAD` names a different branch than this grove's default
  branch is refused with exit status `2`.
- The push is a single `git push --atomic` — one invocation, whether it
  carries one refspec or, under `--all-branches`, one per local branch. If the
  receiving end **does not advertise atomic push**, Git refuses before sending
  any ref update; publish reports that, exits with status `2`, and nothing is
  published. `--all-branches` is one atomic push precisely so that a rejected
  branch cannot leave the others half-published.
- Upstream tracking is written explicitly through `git config` and verified,
  never through `push --set-upstream`.
- The run reports success only after re-asking the hosting side, over the
  wire, whether its `HEAD` resolves to this grove's default branch. If it does
  not — for example a target created with an unborn `HEAD` naming another
  branch — the branch is pushed but the grove stays in the publishing state
  and the run exits with status `2`, telling you to set the hosting side's
  default branch by hand. Rerunning after that completes the transaction.
- A rerun reconciles against the receipt. A rerun that names a different URL
  or a different remote name than the receipt records is refused with exit
  status `2`, naming both values. Comparison is exact and byte for byte:
  `https://host/r.git` and `https://host/r` are different URLs.
- Publishing does not rewrite `AGENTS.md`. When the generated guide still says
  the grove is not published, the run says so and names the file, leaving the
  edit to you.

## Safety and exit status

Before lifecycle operations, `git-grove` verifies the platform and Git
version. It refuses repository-redirecting environment variables such as
`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, object-directory overrides, and
Git configuration overlays. `--ignore-unsupported` reports each finding and
records explicit consent to continue with those variables removed from every
Git child process. It does not bypass the platform check, minimum Git version,
or refused clone options.

Exit status `0` means success, `1` means an unexpected Git/filesystem/I/O
failure (including a failed `sync` fetch), `2` means repository state needs a
human decision (including a worktree `sync` left behind or blocked), and `64`
means a usage error or refused unsupported context. `list --porcelain` emits
the versioned, NUL-delimited `git-grove-list-v1` protocol for automation;
`sync` and `publish` have no porcelain output in 0.3.

## Completions and manual

Generate completion code from the installed binary:

```sh
git-grove completion bash > git-grove.bash
git-grove completion zsh > _git-grove
git-grove completion fish > git-grove.fish
```

The manual source is `man/git-grove.1` and can be viewed from a checkout with
`man ./man/git-grove.1`.

## Development and release verification

The project-local `mise.toml` pins Rust and the musl target. Safe verification
does not install or mutate host configuration:

```sh
mise install
mise exec -- cargo fmt --all -- --check
mise exec -- cargo test --all-targets --locked
mise exec -- cargo clippy --all-targets --locked -- -D warnings
mise exec -- cargo build --release --locked --target x86_64-unknown-linux-musl
scripts/package-release.sh 0.3.0 \
  target/x86_64-unknown-linux-musl/release/git-grove dist
```

Release tags must be strict `vX.Y.Z` and match the package version exactly.
For 0.3.0 the uploaded files are
`git-grove_0.3.0_linux_x86_64.tar.gz` and `SHA256SUMS`.
