use super::init::{
    create_bare, escaped_path, normalize_absolute, open_or_create_root, post_mutation_layout_error,
    retain_partial_for, state_conflict, GuardedRunner, RecoveryState,
};
use crate::error::{GroveError, Result};
use crate::git::config::{
    config_key, config_values, configure_upstreams, escaped, list_local_heads, remote_head_branch,
    required, set_config, trim_one_line, validate_refspec_destinations,
};
use crate::git::query;
use crate::git::runner::{GitRunner, Invocation};
use crate::grove::discover::Grove;
use crate::grove::layout;
use crate::grove::metadata::{self, Metadata, PublishState, FORMAT_VERSION};
use crate::policy::clone_options::Verdict;
use bstr::ByteSlice;
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

pub(crate) trait HomeResolver {
    fn current_home(&self) -> Option<PathBuf>;
    fn named_home(&self, user: &OsStr) -> Result<Option<PathBuf>>;
}

struct SystemHomes;

impl HomeResolver for SystemHomes {
    fn current_home(&self) -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    fn named_home(&self, user: &OsStr) -> Result<Option<PathBuf>> {
        let passwd = std::fs::read("/etc/passwd")
            .map_err(|error| GroveError::failure(format!("cannot read /etc/passwd: {error}")))?;
        for line in passwd.split(|byte| *byte == b'\n') {
            let fields = line.split(|byte| *byte == b':').collect::<Vec<_>>();
            if fields.len() >= 7 && fields[0] == user.as_bytes() {
                let home = PathBuf::from(OsString::from_vec(fields[5].to_vec()));
                return Ok(Some(home));
            }
        }
        Ok(None)
    }
}

pub fn derive_directory_name(url: &OsStr) -> Option<OsString> {
    let mut bytes = url.as_bytes();
    while bytes.last() == Some(&b'/') {
        bytes = &bytes[..bytes.len() - 1];
    }
    if let Some(stripped) = bytes.strip_suffix(b"/.git") {
        bytes = stripped;
    } else if let Some(stripped) = bytes.strip_suffix(b".git") {
        bytes = stripped;
    }
    while bytes.last() == Some(&b'/') {
        bytes = &bytes[..bytes.len() - 1];
    }
    let base = bytes.rsplit(|byte| matches!(byte, b'/' | b':')).next()?;
    if base.is_empty() || matches!(base, b"." | b"..") {
        None
    } else {
        Some(OsString::from_vec(base.to_vec()))
    }
}

pub(crate) fn expand_user_path_with(path: &Path, resolver: &dyn HomeResolver) -> Result<PathBuf> {
    let bytes = path.as_os_str().as_bytes();
    if !bytes.starts_with(b"~") {
        return Ok(path.to_path_buf());
    }
    let (user, rest) = match bytes.iter().position(|byte| *byte == b'/') {
        Some(slash) => (&bytes[1..slash], &bytes[slash + 1..]),
        None => (&bytes[1..], &b""[..]),
    };
    if rest.starts_with(b"/") || user.iter().any(|byte| matches!(byte, b':' | b'\0' | b'\\')) {
        return Err(GroveError::usage(format!(
            "cannot expand malformed user path {}",
            bytes.escape_bytes()
        )));
    }
    let home = if user.is_empty() {
        resolver.current_home()
    } else {
        resolver.named_home(OsStr::from_bytes(user))?
    }
    .ok_or_else(|| {
        GroveError::usage(format!(
            "cannot find a home directory for {}",
            if user.is_empty() {
                "the current user".to_string()
            } else {
                user.escape_bytes().to_string()
            }
        ))
    })?;
    if rest.is_empty() {
        Ok(home)
    } else {
        Ok(home.join(OsStr::from_bytes(rest)))
    }
}

fn validate_clone_postconditions(
    runner: &dyn GitRunner,
    bare: &Path,
    requested_url: &OsStr,
    remote: &OsStr,
) -> Result<Vec<Vec<u8>>> {
    let is_bare = trim_one_line(
        required(
            runner,
            Invocation::new()
                .git_dir(bare)
                .args(["rev-parse", "--is-bare-repository"]),
            "rev-parse --is-bare-repository",
        )?
        .stdout,
        "bare-repository result",
    )?;
    if is_bare != b"true" {
        return Err(state_conflict(
            "the cloned repository is not bare",
            "the partial clone was retained for inspection",
        ));
    }

    let remotes = required(
        runner,
        Invocation::new().git_dir(bare).args(["remote"]),
        "remote list",
    )?;
    let names = remotes
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if names.as_slice() != [remote.as_bytes()] {
        return Err(state_conflict(
            "the clone did not create exactly the requested remote",
            format!(
                "expected {}, found {}",
                escaped(remote.as_bytes()),
                escaped(&remotes.stdout)
            ),
        ));
    }
    let config = bare.join("config");
    let url_key = config_key(b"remote.", remote, b".url");
    let urls = config_values(runner, &config, &url_key)?;
    if urls.as_slice() != [requested_url.as_bytes()] {
        return Err(state_conflict(
            "the clone changed the requested remote URL",
            format!(
                "expected {}, found {}",
                escaped(requested_url.as_bytes()),
                urls.iter()
                    .map(|value| escaped(value))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }

    let fetch_key = config_key(b"remote.", remote, b".fetch");
    let refspecs = config_values(runner, &config, &fetch_key)?;
    validate_refspec_destinations(&refspecs, remote)?;

    required(
        runner,
        Invocation::new()
            .git_dir(bare)
            .args(["rev-parse", "--verify", "HEAD^{commit}"]),
        "rev-parse --verify HEAD",
    )?;

    let object_path = trim_one_line(
        required(
            runner,
            Invocation::new()
                .git_dir(bare)
                .args(["rev-parse", "--git-path", "objects"]),
            "rev-parse --git-path objects",
        )?
        .stdout,
        "object directory",
    )?;
    let object_path = PathBuf::from(OsString::from_vec(object_path));
    let actual_objects = object_path.canonicalize().map_err(|error| {
        GroveError::failure(format!("cannot resolve cloned object directory: {error}"))
    })?;
    let expected_objects = bare.join("objects").canonicalize().map_err(|error| {
        GroveError::failure(format!(
            "cannot resolve held .bare object directory: {error}"
        ))
    })?;
    let canonical_bare = bare
        .canonicalize()
        .map_err(|error| GroveError::failure(format!("cannot resolve held .bare: {error}")))?;
    if actual_objects != expected_objects
        || actual_objects == canonical_bare
        || !actual_objects.starts_with(&canonical_bare)
    {
        return Err(state_conflict(
            "the clone redirected its object directory outside .bare",
            "the partial clone was retained for inspection",
        ));
    }
    match std::fs::symlink_metadata(bare.join("objects/info/alternates")) {
        Ok(_) => {
            return Err(state_conflict(
                "the clone retained an alternate object database",
                "retry with --dissociate or without --reference",
            ))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(GroveError::failure(format!(
                "cannot inspect clone alternates: {error}"
            )))
        }
    }

    let worktrees = required(
        runner,
        Invocation::new()
            .git_dir(bare)
            .args(["worktree", "list", "--porcelain", "-z"]),
        "worktree list --porcelain -z",
    )?;
    let fields = worktrees
        .stdout
        .split(|byte| *byte == b'\0')
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() != 2 || !fields[0].starts_with(b"worktree ") || fields[1] != b"bare" {
        return Err(state_conflict(
            "the cloned repository has incompatible worktree registrations",
            escaped(&worktrees.stdout),
        ));
    }
    let registered = PathBuf::from(OsString::from_vec(fields[0][b"worktree ".len()..].to_vec()));
    if registered.canonicalize().ok().as_ref() != bare.canonicalize().ok().as_ref() {
        return Err(state_conflict(
            "the cloned bare worktree registration points elsewhere",
            escaped(fields[0]),
        ));
    }
    Ok(refspecs)
}

fn repair_refspecs(
    runner: &dyn GitRunner,
    bare: &Path,
    remote: &OsStr,
    narrowed: bool,
    existing: Vec<Vec<u8>>,
) -> Result<Vec<Vec<u8>>> {
    if !existing.is_empty() {
        return Ok(existing);
    }
    let config = bare.join("config");
    let key = config_key(b"remote.", remote, b".fetch");
    let values = if narrowed {
        let heads = list_local_heads(runner, bare)?;
        if heads.is_empty() {
            return Err(state_conflict(
                "the narrowed clone retained no local branches",
                "the partial clone was retained for inspection",
            ));
        }
        heads
            .into_iter()
            .map(|source| {
                let branch = source.strip_prefix(b"refs/heads/").expect("validated head");
                let mut value = b"+refs/heads/".to_vec();
                value.extend_from_slice(branch);
                value.extend_from_slice(b":refs/remotes/");
                value.extend_from_slice(remote.as_bytes());
                value.push(b'/');
                value.extend_from_slice(branch);
                value
            })
            .collect::<Vec<_>>()
    } else {
        let mut value = b"+refs/heads/*:refs/remotes/".to_vec();
        value.extend_from_slice(remote.as_bytes());
        value.extend_from_slice(b"/*");
        vec![value]
    };
    for value in &values {
        set_config(runner, &config, &key, OsStr::from_bytes(value), true)?;
    }
    let written = config_values(runner, &config, &key)?;
    if written != values {
        return Err(GroveError::failure("fetch refspec verification failed"));
    }
    validate_refspec_destinations(&written, remote)?;
    Ok(written)
}

fn resolve_target(url: &OsStr, dir: Option<PathBuf>, cwd: &Path) -> Result<PathBuf> {
    let requested = match dir {
        Some(path) => expand_user_path_with(&path, &SystemHomes)?,
        None => PathBuf::from(derive_directory_name(url).ok_or_else(|| {
            GroveError::usage(format!(
                "cannot derive a directory name from {}",
                escaped(url.as_bytes())
            ))
            .with_detail("pass the directory explicitly")
        })?),
    };
    let requested = if requested.is_absolute() {
        requested
    } else {
        cwd.join(requested)
    };
    normalize_absolute(&requested)
}

struct ClonePlan<'a> {
    url: &'a OsStr,
    root: &'a Path,
    explicit_branch: Option<OsString>,
    verdict: Verdict,
    cwd: &'a Path,
}

fn run_transaction(
    runner: &dyn GitRunner,
    plan: ClonePlan<'_>,
    mutated: &mut bool,
    recovery: &mut RecoveryState,
) -> Result<Grove> {
    let ClonePlan {
        url,
        root: root_path,
        explicit_branch,
        verdict,
        cwd,
    } = plan;
    let root = open_or_create_root(root_path, mutated)?;
    recovery.root = Some(root.identity()?);
    let bare = create_bare(&root, mutated)?;
    recovery.bare = Some(bare.identity()?);
    let guarded = GuardedRunner {
        runner,
        root: &root,
        bare: &bare,
    };

    let mut clone_args = vec![OsString::from("clone"), OsString::from("--bare")];
    clone_args.extend(verdict.forwarded.iter().cloned());
    clone_args.push(OsString::from("--"));
    clone_args.push(url.to_os_string());
    clone_args.push(bare.anchored_path.as_os_str().to_os_string());
    required(
        &guarded,
        Invocation::new().cwd(cwd).args(clone_args),
        "clone --bare",
    )?;
    root.ensure_only_entry(OsStr::new(".bare"))?;
    bare.validate()?;

    let existing =
        validate_clone_postconditions(&guarded, &bare.anchored_path, url, &verdict.remote_name)?;
    let refspecs = repair_refspecs(
        &guarded,
        &bare.anchored_path,
        &verdict.remote_name,
        verdict.narrowed,
        existing,
    )?;
    let config = bare.anchored_path.join("config");
    set_config(
        &guarded,
        &config,
        OsStr::new("worktree.guessRemote"),
        OsStr::new("true"),
        false,
    )?;
    if config_values(&guarded, &config, OsStr::new("worktree.guessRemote"))? != [b"true".as_slice()]
    {
        return Err(GroveError::failure(
            "worktree.guessRemote configuration verification failed",
        ));
    }
    required(
        &guarded,
        Invocation::new().git_dir(&bare.anchored_path).args([
            OsStr::new("fetch"),
            OsStr::new("--prune"),
            OsStr::new("--"),
            verdict.remote_name.as_os_str(),
        ]),
        "fetch --prune",
    )?;
    required(
        &guarded,
        Invocation::new().git_dir(&bare.anchored_path).args([
            OsStr::new("remote"),
            OsStr::new("set-head"),
            OsStr::new("--auto"),
            OsStr::new("--"),
            verdict.remote_name.as_os_str(),
        ]),
        "remote set-head --auto",
    )?;
    let default_branch = remote_head_branch(&guarded, &bare.anchored_path, &verdict.remote_name)?;
    configure_upstreams(
        &guarded,
        &bare.anchored_path,
        &verdict.remote_name,
        &refspecs,
    )?;

    let selected = explicit_branch.unwrap_or(default_branch);
    query::validate_branch_name(&guarded, &selected)?;
    if !query::local_branch_exists(
        &guarded,
        &Grove {
            root: root.anchored_path.clone(),
        },
        &selected,
    )? {
        return Err(state_conflict(
            format!(
                "branch {} was not retained by the clone",
                escaped(selected.as_bytes())
            ),
            "choose a branch present in the cloned repository",
        ));
    }
    let relative_worktree = layout::validate_relative_worktree_path(Path::new(&selected))
        .map_err(post_mutation_layout_error)?;

    let pointer_created = layout::write_pointer_if_absent(&root.anchored_path)?;
    if !pointer_created {
        return Err(state_conflict(
            format!(
                "{} already exists",
                escaped_path(&root.named_path.join(".git"))
            ),
            "the foreign entry was preserved",
        ));
    }
    // A clone has a remote, so its state is `published` — per the
    // specification's `## adopt` step 8, `unpublished` is for a grove with no
    // remote. It deliberately records **no** publication receipt, and not only
    // because it predates the keys: a receipt would match `publish`'s
    // `RepairPublished` rerun path, which rewrites `remote.<name>.fetch` to the
    // wildcard refspec and would therefore silently un-narrow a narrowed clone,
    // whose per-branch refspecs `repair_refspecs` writes on purpose. Staying
    // receipt-less is what keeps a cloned grove out of that path.
    metadata::write_to_config(
        &guarded,
        &config,
        &Metadata {
            version: Some(FORMAT_VERSION),
            default_branch: Some(selected.as_bytes().to_vec().into()),
            remote: Some(verdict.remote_name.as_bytes().to_vec().into()),
            publish_state: PublishState::Published,
            publish_remote: None,
            publish_url: None,
            publish_provider: None,
            publish_owner: None,
            publish_name: None,
        },
    )?;
    let validated =
        layout::validate_worktree_path_at(&root.file, &root.named_path, &relative_worktree)
            .map_err(post_mutation_layout_error)?;
    validated
        .create_parent_directories()
        .map_err(post_mutation_layout_error)?;
    root.validate()?;
    bare.validate()?;
    let worktree_path = validated.path();
    let anchored_worktree = root.anchored_path.join(validated.relative());
    validated
        .validate_vacant()
        .map_err(post_mutation_layout_error)?;
    required(
        &guarded,
        Invocation::new().git_dir(&bare.anchored_path).args([
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--"),
            anchored_worktree.as_os_str(),
            selected.as_os_str(),
        ]),
        "worktree add",
    )?;
    root.validate()?;
    bare.validate()?;
    let grove = Grove::at(&root.named_path).map_err(|error| {
        state_conflict(
            format!("{} changed during cloning", escaped_path(&root.named_path)),
            error.to_string(),
        )
    })?;
    println!("ready: {}", escaped_path(&grove.root));
    println!("next: cd {}", escaped_path(&worktree_path));
    Ok(grove)
}

pub fn run(
    runner: &dyn GitRunner,
    url: &OsStr,
    dir: Option<PathBuf>,
    branch: Option<OsString>,
    verdict: Verdict,
    cwd: &Path,
) -> Result<Grove> {
    if url.as_bytes().is_empty() {
        return Err(GroveError::usage("the repository URL is empty"));
    }
    let root = resolve_target(url, dir, cwd)?;
    if let Some(branch) = &branch {
        query::validate_branch_name(runner, branch)?;
        layout::validate_relative_worktree_path(Path::new(branch))?;
    }
    let mut mutated = false;
    let mut recovery = RecoveryState::default();
    let plan = ClonePlan {
        url,
        root: &root,
        explicit_branch: branch,
        verdict,
        cwd,
    };
    match run_transaction(runner, plan, &mut mutated, &mut recovery) {
        Err(error) if mutated => Err(retain_partial_for(error, &root, &recovery, "clone")),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::runner::{GitOutput, GitRunner, Invocation, RealGit};
    use crate::policy::clone_options;
    use std::cell::RefCell;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::process::Command;

    struct Homes;

    impl HomeResolver for Homes {
        fn current_home(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/home/current"))
        }

        fn named_home(&self, user: &OsStr) -> Result<Option<PathBuf>> {
            Ok((user == "alice").then(|| PathBuf::from("/srv/alice")))
        }
    }

    #[test]
    fn derives_raw_directory_names_like_git_without_dot_git() {
        for (url, expected) in [
            (
                b"git@github.com:user/repo.git".as_slice(),
                b"repo".as_slice(),
            ),
            (b"https://host/group/repo/", b"repo"),
            (b"ssh://host/group/repo.git/", b"repo"),
            (b"/srv/repo.git", b"repo"),
            (b"file:///srv/repo/.git", b"repo"),
            (b"host:path/repo-\xff.git", b"repo-\xff"),
        ] {
            assert_eq!(
                derive_directory_name(OsStr::from_bytes(url))
                    .unwrap()
                    .as_bytes(),
                expected
            );
        }
        for invalid in [b"".as_slice(), b"/", b"/.git", b".", b".."] {
            assert!(derive_directory_name(OsStr::from_bytes(invalid)).is_none());
        }
    }

    #[test]
    fn expands_current_and_named_users_without_process_global_state() {
        assert_eq!(
            expand_user_path_with(Path::new("~/src"), &Homes).unwrap(),
            Path::new("/home/current/src")
        );
        assert_eq!(
            expand_user_path_with(Path::new("~alice/src"), &Homes).unwrap(),
            Path::new("/srv/alice/src")
        );
        assert_eq!(
            expand_user_path_with(Path::new("./src"), &Homes).unwrap(),
            Path::new("./src")
        );
        assert_eq!(
            expand_user_path_with(Path::new(OsStr::from_bytes(b"~/x-\xff")), &Homes)
                .unwrap()
                .as_os_str()
                .as_bytes(),
            b"/home/current/x-\xff"
        );
    }

    #[test]
    fn rejects_unknown_or_malformed_user_expansions() {
        assert_eq!(
            expand_user_path_with(Path::new("~alice"), &Homes).unwrap(),
            Path::new("/srv/alice")
        );
        for path in ["~missing/x", "~alice//x", "~a:b/x"] {
            assert_eq!(
                expand_user_path_with(Path::new(path), &Homes)
                    .unwrap_err()
                    .class,
                crate::error::ExitClass::Usage
            );
        }
    }

    #[test]
    fn preserves_non_utf8_named_user_lookup() {
        struct Raw;
        impl HomeResolver for Raw {
            fn current_home(&self) -> Option<PathBuf> {
                None
            }
            fn named_home(&self, user: &OsStr) -> Result<Option<PathBuf>> {
                assert_eq!(user.as_bytes(), b"u\xff");
                Ok(Some(PathBuf::from(OsString::from_vec(
                    b"/raw/home".to_vec(),
                ))))
            }
        }

        let path = PathBuf::from(OsString::from_vec(b"~u\xff/repo".to_vec()));
        assert_eq!(
            expand_user_path_with(&path, &Raw).unwrap(),
            Path::new("/raw/home/repo")
        );
    }

    #[derive(Clone, Copy)]
    enum Trigger {
        Clone,
        RemoteSetHead,
    }

    struct AfterGit {
        real: RealGit,
        trigger: Trigger,
        action: RefCell<Option<Box<dyn FnOnce()>>>,
    }

    impl GitRunner for AfterGit {
        fn run(&self, invocation: Invocation) -> Result<GitOutput> {
            let argv = invocation.argv_os();
            let matches = match self.trigger {
                Trigger::Clone => argv.first().is_some_and(|argument| argument == "clone"),
                Trigger::RemoteSetHead => argv
                    .windows(2)
                    .any(|arguments| arguments[0] == "remote" && arguments[1] == "set-head"),
            };
            let output = self.real.run(invocation)?;
            if matches && output.ok() {
                if let Some(action) = self.action.borrow_mut().take() {
                    action();
                }
            }
            Ok(output)
        }
    }

    fn git(cwd: &Path, args: &[&OsStr]) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(["-c", "init.defaultBranch=main", "-c", "core.hooksPath="])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", output.stderr.escape_bytes());
    }

    fn origin(parent: &Path) -> PathBuf {
        let origin = parent.join("origin.git");
        let seed = parent.join("seed");
        git(
            parent,
            &[
                OsStr::new("init"),
                OsStr::new("--quiet"),
                OsStr::new("--bare"),
                origin.as_os_str(),
            ],
        );
        git(
            parent,
            &[
                OsStr::new("clone"),
                OsStr::new("--quiet"),
                origin.as_os_str(),
                seed.as_os_str(),
            ],
        );
        std::fs::write(seed.join("README"), b"seed\n").unwrap();
        git(&seed, &[OsStr::new("add"), OsStr::new("README")]);
        git(
            &seed,
            &[
                OsStr::new("commit"),
                OsStr::new("--quiet"),
                OsStr::new("-m"),
                OsStr::new("seed"),
            ],
        );
        git(
            &seed,
            &[
                OsStr::new("push"),
                OsStr::new("--quiet"),
                OsStr::new("origin"),
                OsStr::new("main"),
            ],
        );
        origin
    }

    fn execute_with_action(
        parent: &Path,
        target: &Path,
        trigger: Trigger,
        action: impl FnOnce() + 'static,
    ) -> Result<Grove> {
        let origin = origin(parent);
        let runner = AfterGit {
            real: RealGit::new(),
            trigger,
            action: RefCell::new(Some(Box::new(action))),
        };
        run(
            &runner,
            origin.as_os_str(),
            Some(target.to_path_buf()),
            None,
            clone_options::classify(&[]).unwrap(),
            parent,
        )
    }

    fn run_with_after_clone(
        parent: &Path,
        target: &Path,
        action: impl FnOnce() + 'static,
    ) -> GroveError {
        execute_with_action(parent, target, Trigger::Clone, action).unwrap_err()
    }

    fn assert_layout_was_not_written(target: &Path) {
        assert!(!target.join(".git").exists());
        assert!(!target.join("AGENTS.md").exists());
        assert!(!target.join("CLAUDE.md").exists());
        assert!(!target.join("main").exists());
    }

    #[test]
    fn replacing_the_named_root_cannot_redirect_clone_writes() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("grove");
        let moved = parent.path().join("moved-grove");
        let action_target = target.clone();
        let action_moved = moved.clone();
        let error = run_with_after_clone(parent.path(), &target, move || {
            std::fs::rename(&action_target, &action_moved).unwrap();
            std::fs::create_dir(&action_target).unwrap();
            std::fs::write(action_target.join("foreign"), b"mine").unwrap();
        });

        assert_eq!(error.class, crate::error::ExitClass::NeedsDecision);
        assert_eq!(std::fs::read(target.join("foreign")).unwrap(), b"mine");
        assert!(moved.join(".bare/HEAD").is_file());
        assert!(error.to_string().contains("not a safe cleanup target"));
    }

    #[test]
    fn replacing_the_named_bare_cannot_redirect_clone_writes() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("grove");
        let action_target = target.clone();
        let error = run_with_after_clone(parent.path(), &target, move || {
            std::fs::rename(
                action_target.join(".bare"),
                action_target.join("moved-bare"),
            )
            .unwrap();
            std::fs::create_dir(action_target.join(".bare")).unwrap();
            std::fs::write(action_target.join(".bare/foreign"), b"mine").unwrap();
        });

        assert_eq!(error.class, crate::error::ExitClass::NeedsDecision);
        assert_eq!(
            std::fs::read(target.join(".bare/foreign")).unwrap(),
            b"mine"
        );
        assert!(target.join("moved-bare/HEAD").is_file());
        assert!(error.to_string().contains("not a safe cleanup target"));
    }

    #[test]
    fn concurrent_foreign_root_entry_is_preserved_and_stops_layout_writes() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("grove");
        let action_target = target.clone();
        let error = run_with_after_clone(parent.path(), &target, move || {
            std::fs::write(action_target.join("foreign"), b"mine").unwrap();
        });

        assert_eq!(error.class, crate::error::ExitClass::NeedsDecision);
        assert_eq!(std::fs::read(target.join("foreign")).unwrap(), b"mine");
        assert!(!target.join(".git").exists());
    }

    #[test]
    fn rejects_an_object_directory_symlinked_outside_held_bare() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("grove");
        let external = parent.path().join("external-objects");
        let action_target = target.clone();
        let action_external = external.clone();
        let error = run_with_after_clone(parent.path(), &target, move || {
            std::fs::rename(action_target.join(".bare/objects"), &action_external).unwrap();
            std::os::unix::fs::symlink(&action_external, action_target.join(".bare/objects"))
                .unwrap();
        });

        assert_eq!(error.class, crate::error::ExitClass::NeedsDecision);
        assert!(error.message.contains("object directory outside .bare"));
        assert!(external.is_dir());
    }

    #[test]
    fn dangling_remote_head_target_stops_before_layout_writes() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("grove");
        let action_target = target.clone();
        let error =
            execute_with_action(parent.path(), &target, Trigger::RemoteSetHead, move || {
                git(
                    &action_target,
                    &[
                        OsStr::new("--git-dir"),
                        action_target.join(".bare").as_os_str(),
                        OsStr::new("update-ref"),
                        OsStr::new("-d"),
                        OsStr::new("refs/remotes/origin/main"),
                    ],
                );
            })
            .unwrap_err();

        assert_eq!(error.class, crate::error::ExitClass::Failure);
        assert!(error.message.contains("remote HEAD target"));
        assert_layout_was_not_written(&target);
    }

    #[test]
    fn malformed_clone_postconditions_stop_before_layout_writes() {
        #[derive(Clone, Copy, Debug)]
        enum Fault {
            WrongRemote,
            MultipleRemotes,
            WrongUrl,
            InvalidRefspec,
            WrongRemoteHead,
            ExtraWorktree,
        }

        for fault in [
            Fault::WrongRemote,
            Fault::MultipleRemotes,
            Fault::WrongUrl,
            Fault::InvalidRefspec,
            Fault::WrongRemoteHead,
            Fault::ExtraWorktree,
        ] {
            let parent = tempfile::tempdir().unwrap();
            let target = parent.path().join("grove");
            let action_target = target.clone();
            let external_worktree = parent.path().join("foreign-worktree");
            let trigger = if matches!(fault, Fault::WrongRemoteHead) {
                Trigger::RemoteSetHead
            } else {
                Trigger::Clone
            };
            let error = execute_with_action(parent.path(), &target, trigger, move || {
                let bare = action_target.join(".bare");
                let config = bare.join("config");
                match fault {
                    Fault::WrongRemote => git(
                        &action_target,
                        &[
                            OsStr::new("--git-dir"),
                            bare.as_os_str(),
                            OsStr::new("remote"),
                            OsStr::new("rename"),
                            OsStr::new("origin"),
                            OsStr::new("elsewhere"),
                        ],
                    ),
                    Fault::MultipleRemotes => git(
                        &action_target,
                        &[
                            OsStr::new("--git-dir"),
                            bare.as_os_str(),
                            OsStr::new("remote"),
                            OsStr::new("add"),
                            OsStr::new("extra"),
                            OsStr::new("/unused"),
                        ],
                    ),
                    Fault::WrongUrl => git(
                        &action_target,
                        &[
                            OsStr::new("config"),
                            OsStr::new("--file"),
                            config.as_os_str(),
                            OsStr::new("remote.origin.url"),
                            OsStr::new("/wrong"),
                        ],
                    ),
                    Fault::InvalidRefspec => git(
                        &action_target,
                        &[
                            OsStr::new("config"),
                            OsStr::new("--file"),
                            config.as_os_str(),
                            OsStr::new("remote.origin.fetch"),
                            OsStr::new("+refs/heads/*:refs/heads/*"),
                        ],
                    ),
                    Fault::WrongRemoteHead => git(
                        &action_target,
                        &[
                            OsStr::new("--git-dir"),
                            bare.as_os_str(),
                            OsStr::new("symbolic-ref"),
                            OsStr::new("refs/remotes/origin/HEAD"),
                            OsStr::new("refs/heads/main"),
                        ],
                    ),
                    Fault::ExtraWorktree => git(
                        &action_target,
                        &[
                            OsStr::new("--git-dir"),
                            bare.as_os_str(),
                            OsStr::new("worktree"),
                            OsStr::new("add"),
                            OsStr::new("--detach"),
                            external_worktree.as_os_str(),
                            OsStr::new("HEAD"),
                        ],
                    ),
                }
            })
            .unwrap_err();

            assert!(
                matches!(
                    error.class,
                    crate::error::ExitClass::Failure | crate::error::ExitClass::NeedsDecision
                ),
                "fault {fault:?}: {error}"
            );
            assert_layout_was_not_written(&target);
        }
    }

    #[test]
    fn preserves_a_valid_preexisting_fetch_refspec() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("grove");
        let action_target = target.clone();
        execute_with_action(parent.path(), &target, Trigger::Clone, move || {
            let config = action_target.join(".bare/config");
            git(
                &action_target,
                &[
                    OsStr::new("config"),
                    OsStr::new("--file"),
                    config.as_os_str(),
                    OsStr::new("remote.origin.fetch"),
                    OsStr::new("+refs/heads/main:refs/remotes/origin/main"),
                ],
            );
        })
        .unwrap();

        let values = std::process::Command::new("git")
            .args([
                OsStr::new("config"),
                OsStr::new("--file"),
                target.join(".bare/config").as_os_str(),
                OsStr::new("--get-all"),
                OsStr::new("remote.origin.fetch"),
            ])
            .output()
            .unwrap();
        assert!(values.status.success());
        assert_eq!(
            values.stdout,
            b"+refs/heads/main:refs/remotes/origin/main\n"
        );
    }
}
