---
name: git-grove
description: "Manage a git-grove: one bare repository shared by sibling worktree directories, one per branch, with no working tree at the root. Use when working inside a git-grove layout, deciding where to add or remove a worktree, or checking whether a path is grove metadata rather than repository content."
---

# git-grove

git-grove keeps one bare repository and any number of ordinary Git worktrees side by side under a single root directory:

```text
<grove-root>/
├── .bare/         shared bare repository — every worktree's objects and refs live here
├── .git           a file, not a directory, pointing worktrees at .bare
└── <branch>/      one worktree per checked-out branch
```

A branch can be checked out in only one worktree at a time. `.bare/`, `.git`, and every worktree's own admin directory are outside every worktree and outside repository history — never track or commit any of them.

## Rules

- Never edit or delete anything inside `.bare/`. It is the shared object store and ref database for every worktree in the grove; corrupting it breaks every worktree at once, not just one.
- Never edit or replace the root `.git` file. It is the pointer every worktree-discovery step reads to find `.bare/`.
- A worktree directory is an ordinary Git working tree. Read, edit, commit, and run tests inside it exactly as you would in any other checkout.

## Commands

`git-grove <command>` (visible alias in parentheses where one exists):

- `clone <url> [dir] [-b branch]` (plant) — clone a remote repository into a new grove.
- `init [dir] [-b branch]` (seed) — create a brand-new repository directly as a grove.
- `adopt [path] [--remote NAME] [--default-branch NAME]` (transplant) — convert an ordinary repository already on disk into a grove in place. `adopt --continue` resumes an interrupted adoption; `adopt --abort` reverses one.
- `add <branch> [dir] [--start POINT]`, or `add --detach <revision> [dir]` (sprout) — add a worktree for a branch, or a detached checkout of a revision.
- `list [--porcelain]` (survey) — show the grove root and the state of every worktree.
- `sync` (tend) — fetch and fast-forward every eligible worktree.
- `publish <url>`, or `publish --create OWNER/NAME --host <github|gitlab>` (propagate) — give an unpublished grove a remote and push it, optionally creating the hosting-side repository first.
- `completion <shell>` — generate shell completion code for `bash`, `zsh`, or `fish`.
- `setup --agent <claude|codex|copilot>` — write a project-local hook, scoped to the current worktree, that denies any Edit/Write/Bash/`apply_patch` tool call whose target resolves under `.bare` or the root `.git` file. `claude` and `copilot` share one file; run either once, not both. Each agent's own trust and discovery rules still apply — `setup`'s own output names the exact next step.
- `--skill` — print this document and exit, before any other work.

## Reconstruction escape hatch

git-grove is ordinary Git underneath. If the tool itself is unavailable, the same layout can always be inspected or repaired with plain Git:

```bash
git --git-dir=<grove-root>/.bare worktree list
git --git-dir=<grove-root>/.bare worktree add <path> <branch>
git --git-dir=<grove-root>/.bare worktree remove <path>
```
