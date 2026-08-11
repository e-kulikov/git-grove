# git-grove

`git-grove` manages a repository as a bare clone surrounded by Git worktrees.
Version 0.1 deliberately has a small lifecycle: create or clone a grove, add
worktrees, and inspect their state.

## Requirements

- 64-bit Linux (`x86_64`)
- Git 2.47 or newer

The release binary is statically linked and does not require a system Rust
installation.

## Install

With [mise](https://mise.jdx.dev/dev-tools/backends/github.html) and GitHub as
the backend:

```sh
mise use -g 'github:e-kulikov/git-grove@0.1.0'
```

For declarative mise configuration:

```toml
[tools]
"github:e-kulikov/git-grove" = { version = "0.1.0", asset_pattern = "git-grove_{{ version }}_linux_x86_64.tar.gz", strip_components = 1 }
```

The GitHub backend can install this only after the `v0.1.0` release and its
assets have been published. The release workflow produces an attestation and
`SHA256SUMS`; mise can lock the published checksum with `mise lock`.

For a direct installation, download
`git-grove_0.1.0_linux_x86_64.tar.gz` and `SHA256SUMS` from the GitHub Release,
verify the archive, then install its binary:

```sh
sha256sum --check SHA256SUMS
tar -xzf git-grove_0.1.0_linux_x86_64.tar.gz
install -m 0755 git-grove_0.1.0_linux_x86_64/git-grove ~/.local/bin/git-grove
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
```

The aliases are `plant` for `clone`, `seed` for `init`, `sprout` for `add`,
and `survey` for `list`. Inside a grove, invoking `git grove` without a command
runs `list`. A repository locator as the first argument selects `clone`.

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
├── AGENTS.md    generated repository facts and 0.1 command guide
├── CLAUDE.md    relative link to AGENTS.md
├── main/        worktree
└── feature/     another worktree
```

Grove metadata is stored in the real Git configuration at `.bare/config`, not
in a separate metadata file. Version 0.1 uses `grove.version`,
`grove.defaultBranch`, `grove.remote`, and `grove.publishState`.

All managed worktree paths must remain strictly below the grove root. Existing
files, symlinks, nonempty destinations, ambiguous remote branches, and
unrecognized repository state are preserved for a human decision.

## Safety and exit status

Before lifecycle operations, `git-grove` verifies the platform and Git
version. It refuses repository-redirecting environment variables such as
`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, object-directory overrides, and
Git configuration overlays. `--ignore-unsupported` reports each finding and
records explicit consent to continue with those variables removed from every
Git child process. It does not bypass the platform check, minimum Git version,
or refused clone options.

Exit status `0` means success, `1` means an unexpected Git/filesystem/I/O
failure, `2` means repository state needs a human decision, and `64` means a
usage error or refused unsupported context. `list --porcelain` emits the
versioned, NUL-delimited `git-grove-list-v1` protocol for automation.

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
scripts/package-release.sh 0.1.0 \
  target/x86_64-unknown-linux-musl/release/git-grove dist
```

Release tags must be strict `vX.Y.Z` and match the package version exactly.
For 0.1.0 the uploaded files are
`git-grove_0.1.0_linux_x86_64.tar.gz` and `SHA256SUMS`.
