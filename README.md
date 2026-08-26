# git-grove

`git-grove` manages a repository as a bare clone surrounded by Git worktrees.
It has a small, explicit lifecycle of commands: clone or initialize a grove,
adopt an ordinary repository, add worktrees, inspect their state, fetch and
fast-forward eligible worktrees, and publish an unpublished grove — either to
an existing remote URL, or by creating the hosting-side repository first
through `gh`/`glab`.

## Requirements

- 64-bit Linux (`x86_64`)
- Git 2.47 or newer

The release binary is statically linked and does not require a system Rust
installation.

## Install

With [mise](https://mise.jdx.dev/dev-tools/backends/github.html) and GitHub as
the backend:

```sh
mise use -g 'github:e-kulikov/git-grove@0.5.0'
```

For declarative mise configuration:

```toml
[tools]
"github:e-kulikov/git-grove" = { version = "0.5.0", asset_pattern = "git-grove_{{ version }}_linux_x86_64.tar.gz", strip_components = 1 }
```

The GitHub backend can install this only after the `v0.5.0` release and its
assets have been published. The release workflow produces an attestation and
`SHA256SUMS`; mise can lock the published checksum with `mise lock`.

For a direct installation, download
`git-grove_0.5.0_linux_x86_64.tar.gz` and `SHA256SUMS` from the GitHub Release,
verify the archive, then install its binary:

```sh
sha256sum --check SHA256SUMS
tar -xzf git-grove_0.5.0_linux_x86_64.tar.gz
install -m 0755 git-grove_0.5.0_linux_x86_64/git-grove ~/.local/bin/git-grove
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

# Or convert an existing ordinary, single-worktree repository in place.
git grove adopt ../ordinary-project

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
argument selects `clone`. `transplant` remains a hidden compatibility alias for
`adopt`.

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
├── AGENTS.md    generated repository facts and 0.4 command guide
├── CLAUDE.md    relative link to AGENTS.md
├── main/        worktree
└── feature/     another worktree
```

Grove metadata is stored in the real Git configuration at `.bare/config`, not
in a separate metadata file: `grove.version`, `grove.defaultBranch`,
`grove.remote`, `grove.publishState`, `grove.publishRemote`,
`grove.publishUrl`, and, for a grove published through `publish --create`,
`grove.publishProvider`, `grove.publishOwner`, and `grove.publishName`.

All managed worktree paths must remain strictly below the grove root. Existing
files, symlinks, nonempty destinations, ambiguous remote branches, and
unrecognized repository state are preserved for a human decision.

An adopted repository keeps its current checkout byte-for-byte as the payload
worktree. If that checkout is on `topic` while `main` is selected as the
default, the resulting tree is:

```text
ordinary-project/
├── .bare/
├── .git
├── AGENTS.md
├── CLAUDE.md
├── main/         generated default worktree
└── topic/        adopted payload, including staged and untracked state
```

## Adopt

`git grove adopt [path]` (hidden alias `transplant`) converts an ordinary
repository in place without fetching or checking out over its current files:

```sh
git grove adopt ./project
git grove adopt --remote origin --default-branch main ./project
```

Adopt requires a real `.git` directory, exactly one worktree, a quiescent
repository with no operation markers, lock files, conflict stages, sparse
checkout, initialized submodules, hard-linked payload files, or nested mount
boundaries, and unambiguous remote/default-branch choices. It preserves raw
path bytes, the index and staged state, ignored and untracked files, worktree
private Git state, refs, reflogs, and configuration. Existing unsupported or
ambiguous state is refused before mutation.

Before the first layout mutation, adopt durably creates one
`.grove-adopt-<nonce>/` transaction. An interrupted run prints exact recovery
commands; use only the command matching your intent:

```sh
git grove adopt --continue ./project
git grove adopt --abort ./project
```

Recovery validates the journal, repository identity, and exact before/after
evidence. It never guesses through a corrupt journal or overwrites manual edits.
SIGINT, SIGTERM, and SIGHUP are forwarded to an active Git child and exit as
`128 + signal` after durable reconciliation; SIGKILL is recovered on the next
explicit continue or abort. `GIT_GROVE_FAILPOINT` exists only in test-feature
builds and is inert in the shipped binary.

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

`publish --create` creates the hosting-side repository first, then publishes
to it:

```sh
git grove publish --create my-org/my-project --host github
git grove publish --create my-org/my-project --host gitlab --public
```

It shells out to `gh repo create` or `glab repo create` — never a provider
API — then hands off to the same push/verification machinery a bare
`publish <url>` uses, from the point the URL exists. `<owner>/<name>` must
parse as exactly two non-empty, slash-separated components. `--host` is
required with `--create` and selects the provider: `github` always targets
`github.com`, `gitlab` always targets `gitlab.com` — self-hosted and
enterprise instances of either are out of scope, and neither is ever inferred
from a locally configured host. The created repository is private unless
`--public` is given, which requires `--create`.

Before creating anything, `--create` checks the installed `gh`/`glab` version
against a declared minimum (refused at exit `64` if older), the same local
preflight a bare `publish` performs, and authentication with `gh auth
status`/`glab auth status` (refused at exit `2`, phrased as "could not
confirm authentication" rather than "not authenticated," since both commands
validate a stored token over the network). While the repository is being
created, the grove records the requested provider, owner, name, and remote
under a transient `creating` state; rerunning `publish --create` against a
grove already `creating` resumes the same request rather than creating
anything twice, and a bare `publish <url>` against such a grove refuses at
exit `2`, naming the recorded owner/name, rather than silently reinterpreting
it. If the hosting side already has a matching, empty repository under that
name — typically an earlier attempt that created it but did not finish
publishing — publication proceeds using it instead of failing with "already
exists"; a repository under that name that does not match is refused at exit
`2` as an unrelated existing repository. `--create` never asks a provider to
delete, rename, or transfer a repository, including one it created itself.

Publishing is a transaction with four states — `unpublished`, `creating`,
`publishing`, and `published` — recorded in `grove.publishState`. Its receipt,
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
  host-created commit, and never deletes a remote ref. Only `--create` ever
  calls a hosting provider, and it shells out to `gh`/`glab` — never a
  provider API — and never to delete, rename, or transfer anything.
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
failure (including a failed `sync` fetch, or a provider CLI that is absent or
whose output cannot be parsed), `2` means repository state needs a human
decision (including a worktree `sync` left behind or blocked, unconfirmed
provider authentication, or a `--create` receipt conflict), and `64` means a
usage error, refused unsupported context, or a provider CLI older than the
declared minimum. `list --porcelain` emits the versioned, NUL-delimited
`git-grove-list-v1` protocol for automation; `sync`, `adopt`, and `publish`
have no porcelain output.

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
scripts/package-release.sh 0.5.0 \
  target/x86_64-unknown-linux-musl/release/git-grove dist
```

Release tags must be strict `vX.Y.Z` and match the package version exactly.
For 0.5.0 the uploaded files are
`git-grove_0.5.0_linux_x86_64.tar.gz` and `SHA256SUMS`.
